#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::yield_now;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use embassy_net_w5500::*;
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::{PIN_17, PIN_20, PIN_21, SPI0};
use embassy_rp::spi::{Async, Config as SpiConfig, Spi};
use embedded_hal_async::spi::ExclusiveDevice;
use embedded_io::asynch::Read;
use embedded_io::asynch::Write;
use heapless::Vec;
use rand::RngCore;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

macro_rules! singleton {
    ($val:expr) => {{
        type T = impl Sized;
        static STATIC_CELL: StaticCell<T> = StaticCell::new();
        let (x,) = STATIC_CELL.init(($val,));
        x
    }};
}

#[embassy_executor::task]
async fn ethernet_task(
    runner: Runner<
        'static,
        ExclusiveDevice<Spi<'static, SPI0, Async>, Output<'static, PIN_17>>,
        Input<'static, PIN_21>,
        Output<'static, PIN_20>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<Device<'static>>) -> ! {
    stack.run().await
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    let mut rng = RoscRng;

    let mut spi_cfg = SpiConfig::default();
    spi_cfg.frequency = 50_000_000;
    let (miso, mosi, clk) = (p.PIN_16, p.PIN_19, p.PIN_18);
    let spi = Spi::new(p.SPI0, clk, mosi, miso, p.DMA_CH0, p.DMA_CH1, spi_cfg);
    let cs = Output::new(p.PIN_17, Level::High);
    let w5500_int = Input::new(p.PIN_21, Pull::Up);
    let w5500_reset = Output::new(p.PIN_20, Level::High);

    let mac_addr = [0x02, 0x00, 0x00, 0x00, 0x00, 0x00];
    let state = singleton!(State::<8, 8>::new());
    let (device, runner) = embassy_net_w5500::new(
        mac_addr,
        state,
        ExclusiveDevice::new(spi, cs),
        w5500_int,
        w5500_reset,
    )
    .await;
    unwrap!(spawner.spawn(ethernet_task(runner)));

    // Generate random seed
    let seed = rng.next_u64();

    // Init network stack
    let stack = &*singleton!(Stack::new(
        device,
        embassy_net::Config::Dhcp(Default::default()),
        singleton!(StackResources::<3>::new()),
        seed
    ));

    // Launch network task
    unwrap!(spawner.spawn(net_task(stack)));

    info!("Waiting for DHCP...");
    let cfg = wait_for_config(stack).await;
    let local_addr = cfg.address.address();
    info!("IP address: {:?}", local_addr);

    // Create two sockets listening to the same port, to handle simultaneous connections
    unwrap!(spawner.spawn(listen_task(stack, 0, 1234)));
    unwrap!(spawner.spawn(listen_task(stack, 1, 1234)));
}

#[embassy_executor::task(pool_size = 2)]
async fn listen_task(stack: &'static Stack<Device<'static>>, id: u8, port: u16) {
    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    loop {
        let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(embassy_net::SmolDuration::from_secs(10)));

        info!("SOCKET {}: Listening on TCP:{}...", id, port);
        if let Err(e) = socket.accept(5201).await {
            warn!("accept error: {:?}", e);
            continue;
        }
        info!(
            "SOCKET {}: Received connection from {:?}",
            id,
            socket.remote_endpoint()
        );

        match process(socket).await {
            Ok(_) => warn!("ok"),
            Err(_) => warn!("err"),
        }
    }
}

async fn wait_for_config(stack: &'static Stack<Device<'static>>) -> embassy_net::StaticConfig {
    loop {
        if let Some(config) = stack.config() {
            return config;
        }
        yield_now().await;
    }
}

// TODO: add NoopMutex around this!
static mut SESSIONS: Vec<[u8; 36], 8> = Vec::new();
static mut JSON_BUFFER: [u8; 1024] = [0; 1024];

// These are used as system state and as messages in original iperf.
// Just using them for messages for now.
#[allow(dead_code)]
mod iperf_command {
    pub const TEST_START: u8 = 1;
    pub const TEST_RUNNING: u8 = 2;
    pub const TEST_END: u8 = 4;
    pub const PARAM_EXCHANGE: u8 = 9;
    pub const CREATE_STREAMS: u8 = 10;
    pub const SERVER_TERMINATE: u8 = 11;
    pub const CLIENT_TERMINATE: u8 = 12;
    pub const EXCHANGE_RESULTS: u8 = 13;
    pub const DISPLAY_RESULTS: u8 = 14;
    pub const IPERF_START: u8 = 15;
    pub const IPERF_DONE: u8 = 16;
}

async fn process(mut socket: TcpSocket<'_>) -> core::result::Result<(), ()> {
    let (mut buf_reader, mut buf_writer) = socket.split();
    // The first thing an iperf3 client will send is
    // a 36 character, nul terminated session id cookie
    let mut session_id_cstr: [u8; 37] = [0; 37];
    let mut session_id: [u8; 36] = [0; 36];
    buf_reader.read_exact(&mut session_id_cstr).await.unwrap();

    if session_id_cstr.is_ascii() {
        // The string is a C style nul terminated thing, so strip that off.
        session_id.copy_from_slice(&session_id_cstr[..36]);
        info!("session_id: {}", core::str::from_utf8(&session_id).unwrap());

        // TODO: sync access to sessions
        let control_channel = unsafe { !SESSIONS.contains(&session_id) };
        if control_channel {
            unsafe { SESSIONS.push(session_id).unwrap() };
            println!("control channel opened");
            println!("ask the client to send the config parameters. We won't use them but instead will print them.");
            buf_writer
                .write_all(&[iperf_command::PARAM_EXCHANGE])
                .await
                .unwrap();
            let mut message_len: [u8; 4] = [0; 4];
            buf_reader.read_exact(&mut message_len).await.unwrap();
            let message_len = u32::from_be_bytes(message_len);
            info!("Config JSON length {}", message_len);

            if message_len > 1024 {
                defmt::panic!("Too much JSON!");
            }
            let buf = unsafe { &mut JSON_BUFFER[0..message_len as usize] };

            buf_reader.read_exact(buf).await.unwrap();
            if buf.is_ascii() {
                info!("Config: {}", core::str::from_utf8(buf).unwrap());
            }

            println!("ask the client to connect to a 2nd socket");
            if buf_writer
                .write_all(&[iperf_command::CREATE_STREAMS])
                .await
                .is_err()
            {
                return Err(());
            };

            println!("ask the client to start the test");
            if buf_writer
                .write_all(&[iperf_command::TEST_START])
                .await
                .is_err()
            {
                return Err(());
            };
            let _ = buf_writer.flush().await;

            info!("tell the client we've started running the test");
            if buf_writer
                .write_all(&[iperf_command::TEST_RUNNING])
                .await
                .is_err()
            {
                return Err(());
            };
            let _ = buf_writer.flush().await;
            let mut reply: [u8; 1] = [0; 1];
            buf_reader.read_exact(&mut reply).await.unwrap();

            // We're hoping that was TEST_END. check:
            if reply[0] == iperf_command::TEST_END {
                info!("TEST_END command received from client");

                // if we had calculated benchmark states, now we would exchange them.
                // // buf_writer.write_all(&[iperf_command::EXCHANGE_RESULTS]);
                // we'll instead instruct the client to tell us their version of events...

                info!("Tell the client to display benchmark results");
                buf_writer
                    .write_all(&[iperf_command::DISPLAY_RESULTS])
                    .await
                    .unwrap();
                buf_writer.flush().await.unwrap();
                info!("Dropping session cookie now");
                unsafe {
                    SESSIONS.retain(|&f| f != session_id);
                }
            } else if reply[0] == iperf_command::IPERF_DONE {
                info!("Expected TEST_END, recieved IPERF_DONE");
            } else {
                info!("Expected TEST_END, recieved {}", reply[0]);
            }

            let mut reply: [u8; 1] = [0; 1];
            match buf_reader.read_exact(&mut reply).await {
                Ok(_) => {
                    if reply[0] == iperf_command::IPERF_DONE {
                        info!("Client says we're good, ship it!");
                    } else {
                        info!("Expected IPERF_DONE, recieved {}", reply[0]);
                    }

                    // No more data is going to arrive, just wait now
                }
                Err(_) => return Err(()),
            };
        } else {
            // this will be the second connection from the client
            println!("data channel opened");

            // assume that the client will be sending us data to consume.
            // keep going until we stop receiving.
            let mut message: [u8; 0x1000] = [0; 0x1000];
            let mut bytes_total: u64 = 0;
            let mut done = false;
            while !done {
                let sz = buf_reader.read(&mut message).await.unwrap();
                if sz == 0 {
                    done = true;
                    info!("received {} bytes:", bytes_total);
                }
                bytes_total += sz as u64;
            }
        };
    };

    Ok(())
}
