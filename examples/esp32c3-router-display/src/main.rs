//! Bare-metal Wi-Fi / ubus dashboard for the round razem-tag gadget.
#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_net::{
    dns::DnsSocket,
    tcp::client::{TcpClient, TcpClientState},
    Runner, Stack, StackResources,
};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_graphics::{pixelcolor::Rgb565, prelude::RgbColor};
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    i2c::master::{Config as I2cConfig, I2c},
    interrupt::software::SoftwareInterruptControl,
    rng::Rng,
    spi::master::{Config as SpiConfig, Spi},
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_radio::wifi::{sta::StationConfig, Config as WifiConfig, Interface, WifiController};
use log::{info, warn};
use reqwless::{
    client::HttpClient,
    headers::ContentType,
    request::{Method, RequestBuilder},
};
use static_cell::StaticCell;
use ubus::BlobMsgTable;
use ubus_router_display::{
    panel, render,
    rpc::{self, Client, Error},
    stats::{Board, Counters, Dashboard, RateTracker, System, Wan, Wifi},
};

const WIFI_SSID: &str = env!("WIFI_SSID", "Source config.sh before building");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD", "Source config.sh before building");
const UBUS_PASSWORD: &str = env!("UBUS_PASSWORD", "Source config.sh before building");
macro_rules! env_or {
    ($name:literal, $default:literal) => {
        match option_env!($name) {
            Some(value) => value,
            None => $default,
        }
    };
}
const UBUS_URL: &str = env_or!("UBUS_URL", "http://192.168.8.1:8080/ubus");
const UBUS_USERNAME: &str = env_or!("UBUS_USERNAME", "dashboard");
const WAN_INTERFACE: &str = env_or!("WAN_INTERFACE", "wan");
const WIFI_DEVICE: &str = env_or!("WIFI_DEVICE", "");

type Shared = Mutex<CriticalSectionRawMutex, Dashboard>;
static STATE: StaticCell<Shared> = StaticCell::new();
static NET: StaticCell<StackResources<4>> = StaticCell::new();
static PIXELS: StaticCell<[Rgb565; panel::WIDTH * panel::TILE_ROWS]> = StaticCell::new();

esp_bootloader_esp_idf::esp_app_desc!();

struct Http {
    stack: Stack<'static>,
}

fn http_error(e: reqwless::Error) -> Error {
    match e {
        reqwless::Error::BufferTooSmall => Error::TooLarge,
        _ => Error::Transport,
    }
}

impl Http {
    async fn exchange(&mut self, request: &[u8], buffer: &mut [u8]) -> Result<usize, Error> {
        let tcp_state = TcpClientState::<1, 1536, 1536>::new();
        let tcp = TcpClient::new(self.stack, &tcp_state);
        let dns = DnsSocket::new(self.stack);
        let mut http = HttpClient::new(&tcp, &dns);
        let len = {
            let mut request = http
                .request(Method::POST, UBUS_URL)
                .await
                .map_err(http_error)?
                .content_type(ContentType::ApplicationJson)
                .headers(&[("Connection", "close")])
                .body(request);
            let response = request.send(buffer).await.map_err(http_error)?;
            if response.status.0 != 200 {
                return Err(Error::Http(response.status.0));
            }
            let body = response.body().read_to_end().await.map_err(http_error)?;
            // reqwless moves the decoded body to the start of this buffer.
            body.len()
        };
        Ok(len)
    }
}

impl rpc::Http for Http {
    async fn post(&mut self, request: &[u8], response: &mut [u8]) -> Result<usize, Error> {
        with_timeout(Duration::from_secs(5), self.exchange(request, response))
            .await
            .map_err(|_| Error::Timeout)?
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    esp_println::logger::init_logger_from_env();
    assert!(
        UBUS_URL.starts_with("http://"),
        "This LAN example requires an http:// ubus URL"
    );
    let p = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    esp_alloc::heap_allocator!(size: 96 * 1024);
    let timers = TimerGroup::new(p.TIMG0);
    esp_rtos::start(
        timers.timer0,
        SoftwareInterruptControl::new(p.SW_INTERRUPT).software_interrupt0,
    );

    // Exact razem-tag board pins. GPIO10 is also touch INT; poll touch instead.
    let _backlight = Output::new(p.GPIO3, Level::High, OutputConfig::default());
    let _cs = Output::new(p.GPIO10, Level::Low, OutputConfig::default());
    let dc = Output::new(p.GPIO2, Level::Low, OutputConfig::default());
    let spi = Spi::new(
        p.SPI2,
        SpiConfig::default().with_frequency(Rate::from_mhz(40)),
    )
    .unwrap()
    .with_sck(p.GPIO6)
    .with_mosi(p.GPIO7);
    let mut panel = panel::Panel::new(spi, dc);
    let mut delay = Delay::new();
    panel.init(&mut delay).unwrap();

    let mut touch = I2c::new(
        p.I2C0,
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .unwrap()
    .with_sda(p.GPIO4)
    .with_scl(p.GPIO5);
    let mut touch_reset = Output::new(p.GPIO1, Level::Low, OutputConfig::default());
    delay.delay_millis(20);
    touch_reset.set_high();
    delay.delay_millis(400);
    let _ = touch.write(0x15, &[0xFE, 1]); // Keep scanning while idle.

    let state = STATE.init(Mutex::new(Dashboard::default()));
    let (controller, interfaces) = esp_radio::wifi::new(p.WIFI, Default::default()).unwrap();
    let mut seed = [0u8; 8];
    Rng::new().read(&mut seed);
    let (stack, runner) = embassy_net::new(
        interfaces.station,
        embassy_net::Config::dhcpv4(Default::default()),
        NET.init(StackResources::new()),
        u64::from_le_bytes(seed),
    );
    spawner.spawn(wifi_connection(controller).unwrap());
    spawner.spawn(network(runner).unwrap());
    spawner.spawn(poll_router(stack, state).unwrap());

    let pixels = PIXELS.init([Rgb565::BLACK; panel::WIDTH * panel::TILE_ROWS]);
    let mut page = 0;
    let mut page_since = Instant::now();
    let mut frame_at = Instant::now();
    let mut was_down = false;
    loop {
        let now = Instant::now();
        let mut fingers = [0];
        let mut changed = false;
        if touch.write_read(0x15, &[0x02], &mut fingers).is_ok() {
            let down = fingers[0] & 0x0F != 0;
            if was_down && !down {
                page = (page + 1) % render::PAGE_COUNT;
                page_since = now;
                changed = true;
            }
            was_down = down;
        }
        if now.duration_since(page_since) >= Duration::from_secs(8) {
            page = (page + 1) % render::PAGE_COUNT;
            page_since = now;
            changed = true;
        }
        if changed || now >= frame_at {
            let snapshot = state.lock().await.clone();
            for y in (0..240).step_by(panel::TILE_ROWS) {
                render::draw(
                    &mut panel::Tile { y, pixels },
                    &snapshot,
                    page,
                    now.as_millis(),
                )
                .unwrap();
                panel.write_tile(y, pixels).unwrap();
                embassy_futures::yield_now().await;
            }
            frame_at = Instant::now() + Duration::from_millis(250);
        }
        Timer::after(Duration::from_millis(20)).await;
    }
}

#[embassy_executor::task]
async fn wifi_connection(mut controller: WifiController<'static>) -> ! {
    controller
        .set_config(&WifiConfig::Station(
            StationConfig::default()
                .with_ssid(WIFI_SSID)
                .with_password(WIFI_PASSWORD.into()),
        ))
        .unwrap();
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                info!("Wi-Fi associated; waiting for DHCP");
                let _ = controller.wait_for_disconnect_async().await;
                warn!("Wi-Fi disconnected; reconnecting");
            }
            Err(e) => warn!("Wi-Fi association failed: {e:?}"),
        }
        Timer::after(Duration::from_secs(3)).await;
    }
}

#[embassy_executor::task]
async fn network(mut runner: Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn poll_router(stack: Stack<'static>, shared: &'static Shared) -> ! {
    let mut client = Client::new(Http { stack }, UBUS_USERNAME, UBUS_PASSWORD);
    let mut state = Dashboard::default();
    let mut tracker = RateTracker::default();
    let object = alloc::format!("network.interface.{WAN_INTERFACE}");
    let mut iteration = 0u32;
    loop {
        if !stack.is_config_up() {
            tracker.reset();
            if state.last_sample_ms.is_some() {
                state.error = Some(Error::Transport);
                state.push_rate(None);
                *shared.lock().await = state.clone();
            }
        }
        stack.wait_config_up().await;
        let start = Instant::now();
        match sample(&mut client, &object, &mut state, &mut tracker, iteration).await {
            Ok(()) => state.error = None,
            Err(e) => {
                warn!("Router sample failed: {e:?}");
                state.error = Some(e);
                state.push_rate(None);
                tracker.reset();
            }
        }
        *shared.lock().await = state.clone();
        iteration = iteration.wrapping_add(1);
        Timer::at(start + Duration::from_secs(1)).await;
    }
}

async fn sample(
    client: &mut Client<Http>,
    object: &str,
    state: &mut Dashboard,
    tracker: &mut RateTracker,
    iteration: u32,
) -> Result<(), Error> {
    let empty = BlobMsgTable::empty();
    state.system = System::parse(&client.call("system", "info", &empty).await?);
    state.last_sample_ms = Some(Instant::now().as_millis());
    state.wan_error = None;
    state.counters = None;
    let mut rate = None;
    match client.call(object, "status", &empty).await {
        Ok(reply) => {
            let wan = Wan::parse(&reply);
            if wan.up == Some(true) {
                if let Some(device) = wan.device.as_deref() {
                    let args = BlobMsgTable::build(|b| b.put_str(Some("name"), device));
                    match client.call("network.device", "status", &args).await {
                        Ok(reply) => {
                            let counts = Counters::parse(&reply);
                            if let Some(uptime) = state.system.uptime {
                                rate = tracker.update(
                                    device,
                                    uptime,
                                    Instant::now().as_millis(),
                                    counts,
                                );
                            }
                            state.counters = Some(counts);
                        }
                        Err(e) => state.wan_error = Some(e),
                    }
                }
            }
            state.wan = Some(wan);
        }
        Err(e) => {
            state.wan = None;
            state.wan_error = Some(e);
        }
    }
    if state.counters.is_none() || state.system.uptime.is_none() {
        tracker.reset();
    }
    state.push_rate(rate);

    // Board identity changes rarely; optional Wi-Fi calls run every five polls.
    if iteration.is_multiple_of(60) {
        if let Ok(reply) = client.call("system", "board", &empty).await {
            state.board = Board::parse(&reply);
        }
    }
    if !WIFI_DEVICE.is_empty() && iteration.is_multiple_of(5) {
        state.wifi = None;
        state.wifi_error = None;
        let args = BlobMsgTable::build(|b| b.put_str(Some("device"), WIFI_DEVICE));
        match client.call("iwinfo", "info", &args).await {
            Ok(reply) => {
                let mut wifi = Wifi::parse(&reply);
                match client.call("iwinfo", "assoclist", &args).await {
                    Ok(reply) => wifi.set_clients(&reply),
                    Err(e) => state.wifi_error = Some(e),
                }
                state.wifi = Some(wifi);
            }
            Err(e) => state.wifi_error = Some(e),
        }
    }
    Ok(())
}
