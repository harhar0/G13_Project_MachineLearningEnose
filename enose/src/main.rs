// =========================================================
// ELECTRONIC NOSE - ESP32-S3 WITH WIFI, MQTT, HTTP & OTA
// Migrated from no_std to std (esp-idf-svc)
// =========================================================
//
// Fitur:
//   - DHT22 sensor (1 buah, GPIO10) untuk suhu & kelembaban
//   - ADS1115 ADC (2 buah) untuk 8 sensor gas
//   - MQTT ke ThingsBoard (telemetri & OTA)
//   - HTTP POST ke Azure IoT Hub
//   - HTTP POST ke MongoDB Atlas Data API
//   - OTA firmware update via ThingsBoard MQTT
//

use std::sync::{
    atomic::{AtomicU32, Ordering},
    mpsc,
};
use std::time::Duration;

use embedded_svc::{
    http::{client::Client as HttpClient, Method},
    io::Write,
    mqtt::client::{Details, EventPayload, QoS},
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        delay::{Ets, FreeRtos},
        gpio::{PinDriver, InputOutput, Pull},
        i2c::{I2cConfig, I2cDriver},
        peripherals::Peripherals,
        uart::{UartConfig, UartDriver},
        units::Hertz,
    },
    mqtt::client::{EspMqttClient, MqttClientConfiguration},
    nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault},
    ota::EspOta,
    wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi},
    http::client::{Configuration as HttpConfiguration, EspHttpConnection},
};

use dht_sensor::{dht22, DhtReading};
use serde::Serialize;
use sha2::{Digest, Sha256};

// =========================================================
// KONFIGURASI - GANTI SESUAI KEBUTUHAN ANDA
// =========================================================

// WiFi Configuration
const WIFI_SSID: &str = "J.3";
const WIFI_PASS: &str = "SDHS5849";

// ThingsBoard MQTT Configuration
const TB_MQTT_HOST: &str = "thingsboard.cloud";  // atau IP server ThingsBoard Anda
const TB_MQTT_PORT: u16 = 1883;
const TB_ACCESS_TOKEN: &str = "MEEpnMMdY4Pr1h2ibSc2";
const TB_TELEMETRY_TOPIC: &str = "v1/devices/me/telemetry";
const TB_ATTRIBUTES_TOPIC: &str = "v1/devices/me/attributes";
// OTA topics (ThingsBoard firmware v2 API)
const TB_FW_RESPONSE_TOPIC: &str = "v2/fw/response/+/chunk/+";
const TB_FW_REQUEST_TOPIC_PREFIX: &str = "v2/fw/request/";
const TB_ATTR_RESPONSE_TOPIC: &str = "v1/devices/me/attributes/response/+";
const TB_ATTR_REQUEST_TOPIC: &str = "v1/devices/me/attributes/request/1";

// Ukuran potongan firmware per request (harus SAMA di setiap request)
const OTA_CHUNK_SIZE: usize = 4096;
// Ukuran maksimum firmware = ukuran partisi ota_0/ota_1 (1800K)
const OTA_MAX_SIZE: usize = 1800 * 1024;

// Azure IoT Hub Configuration
const AZURE_IOT_ENDPOINT: &str = "https://YOUR-IOT-HUB.azure-devices.net/devices/YOUR-DEVICE-ID/messages/events?api-version=2020-03-13";
const AZURE_SAS_TOKEN: &str = "SharedAccessSignature sr=YOUR-IOT-HUB...";

// MongoDB Atlas Data API Configuration
const MONGO_DATA_API_URL: &str = "https://data.mongodb-api.com/app/data-YOUR-APP/endpoint/data/v1/action/insertOne";
const MONGO_API_KEY: &str = "YOUR-MONGO-API-KEY";
const MONGO_DATABASE: &str = "iotdb";
const MONGO_COLLECTION: &str = "sensordata";

// =========================================================
// KONSTANTA HARDWARE
// =========================================================

const ADS1_ADDR: u8 = 0x48;
const ADS2_ADDR: u8 = 0x49;
const ADS_REG_CONVERT: u8 = 0x00;
const ADS_REG_CONFIG: u8 = 0x01;

// Aktifkan setelah URL/token Azure & MongoDB diisi dengan benar
const ENABLE_AZURE: bool = false;
const ENABLE_MONGODB: bool = false;

// Firmware version for OTA
const FW_VERSION: &str = "0.9.0";
const FW_TITLE: &str = "esp32_enose";
// Penanda build: tampil di banner, heartbeat, dan telemetri ThingsBoard
const BUILD_TAG: &str = "ota-test-0.8.0";

// =========================================================
// DATA STRUCTURES
// =========================================================

#[derive(Clone, Default, Serialize)]
struct SensorData {
    mq6: f32,
    mq135: f32,
    mq3: f32,
    mq7: f32,
    tgs2611: f32,
    tgs2602: f32,
    tgs2600: f32,
    tgs2620: f32,
    dht_temp: f32,
    dht_hum: f32,
}

#[derive(Clone, Serialize)]
struct TelemetryPayload {
    sensors: SensorData,
    timestamp: u64,
    fw_version: &'static str,
}

struct SystemState {
    last_sensor_read_ms: u64,
    sensors: SensorData,
}

impl SystemState {
    fn new() -> Self {
        Self {
            last_sensor_read_ms: 0,
            sensors: SensorData::default(),
        }
    }
}

// =========================================================
// HELPER FUNCTIONS
// =========================================================

fn millis() -> u64 {
    (esp_idf_svc::systime::EspSystemTime.now().as_millis()) as u64
}

/// Waktu (ms, 32-bit) saat program mulai berjalan.
/// Xtensa tidak punya atomic 64-bit, jadi dipakai AtomicU32 + wrapping_sub.
static BOOT_MS: AtomicU32 = AtomicU32::new(0);

/// Lama program berjalan sejak boot (ms)
fn uptime_ms() -> u64 {
    (millis() as u32).wrapping_sub(BOOT_MS.load(Ordering::Relaxed)) as u64
}

fn build_telemetry(state: &SystemState) -> TelemetryPayload {
    TelemetryPayload {
        sensors: state.sensors.clone(),
        timestamp: uptime_ms(),
        fw_version: FW_VERSION,
    }
}

/// Cetak data sensor terpisah-pisah agar mudah dibaca di serial monitor
fn log_readable(s: &SensorData) {
    log::info!("[SENSOR  ] Suhu: {:.1} C | Kelembapan: {:.1} %", s.dht_temp, s.dht_hum);
    log::info!("[GAS-MQ  ] MQ6={:.1} | MQ135={:.1} | MQ3={:.1} | MQ7={:.1} mV", s.mq6, s.mq135, s.mq3, s.mq7);
    log::info!("[GAS-TGS ] TGS2611={:.1} | TGS2602={:.1} | TGS2600={:.1} | TGS2620={:.1} mV", s.tgs2611, s.tgs2602, s.tgs2600, s.tgs2620);
}

// =========================================================
// ADS1115 ADC FUNCTIONS
// =========================================================

fn ads_config(channel: u8) -> [u8; 3] {
    let mux: u16 = match channel {
        0 => 0b100,
        1 => 0b101,
        2 => 0b110,
        3 => 0b111,
        _ => 0b100,
    };
    let high: u8 = (0b1_000_000_1u8) | ((mux as u8) << 4);
    let low: u8 = 0b1000_0011;
    [ADS_REG_CONFIG, high, low]
}

fn ads_read_single(
    i2c: &mut I2cDriver<'_>,
    addr: u8,
    channel: u8,
) -> Result<i16, esp_idf_svc::hal::i2c::I2cError> {
    let cfg = ads_config(channel);
    i2c.write(addr, &cfg, 100)?;
    FreeRtos::delay_ms(10);
    i2c.write(addr, &[ADS_REG_CONVERT], 100)?;
    let mut buf = [0u8; 2];
    i2c.read(addr, &mut buf, 100)?;
    Ok(i16::from_be_bytes(buf))
}

fn raw_to_mv(raw: i16) -> f32 {
    (raw as f32) * 6144.0 / 32767.0
}

fn read_gas_sensors(state: &mut SystemState, i2c: &mut I2cDriver<'_>) {
    // Jika pembacaan I2C gagal, tampilkan warning (bukan diam-diam jadi 0)
    let mut rd = |addr: u8, ch: u8, name: &str| -> f32 {
        match ads_read_single(i2c, addr, ch) {
            Ok(raw) => raw_to_mv(raw),
            Err(e) => {
                log::warn!("ADS {} (0x{:02X} ch{}) error: {:?}", name, addr, ch, e);
                0.0
            }
        }
    };

    state.sensors.mq6     = rd(ADS1_ADDR, 0, "MQ6");
    state.sensors.mq135   = rd(ADS1_ADDR, 1, "MQ135");
    state.sensors.mq3     = rd(ADS1_ADDR, 2, "MQ3");
    state.sensors.mq7     = rd(ADS1_ADDR, 3, "MQ7");
    state.sensors.tgs2611 = rd(ADS2_ADDR, 0, "TGS2611");
    state.sensors.tgs2602 = rd(ADS2_ADDR, 1, "TGS2602");
    state.sensors.tgs2600 = rd(ADS2_ADDR, 2, "TGS2600");
    state.sensors.tgs2620 = rd(ADS2_ADDR, 3, "TGS2620");
}

// =========================================================
// DHT22 WRAPPER (embedded-hal 0.2 compatibility)
// =========================================================

struct OldDelay;

impl embedded_hal::blocking::delay::DelayUs<u8> for OldDelay {
    fn delay_us(&mut self, us: u8) {
        Ets::delay_us(us as u32);
    }
}

impl embedded_hal::blocking::delay::DelayUs<u16> for OldDelay {
    fn delay_us(&mut self, us: u16) {
        Ets::delay_us(us as u32);
    }
}

impl embedded_hal::blocking::delay::DelayUs<u32> for OldDelay {
    fn delay_us(&mut self, us: u32) {
        Ets::delay_us(us);
    }
}

impl embedded_hal::blocking::delay::DelayMs<u8> for OldDelay {
    fn delay_ms(&mut self, ms: u8) {
        Ets::delay_ms(ms as u32);
    }
}

impl embedded_hal::blocking::delay::DelayMs<u16> for OldDelay {
    fn delay_ms(&mut self, ms: u16) {
        Ets::delay_ms(ms as u32);
    }
}

impl embedded_hal::blocking::delay::DelayMs<u32> for OldDelay {
    fn delay_ms(&mut self, ms: u32) {
        Ets::delay_ms(ms);
    }
}

fn read_dht22<P: esp_idf_svc::hal::gpio::IOPin>(
    pin: &mut PinDriver<'_, P, InputOutput>,
) -> Option<(f32, f32)> {
    let mut delay = OldDelay;
    match dht22::Reading::read(&mut delay, pin) {
        Ok(reading) => Some((reading.temperature, reading.relative_humidity)),
        Err(e) => {
            log::warn!("DHT22 read error: {:?}", e);
            None
        }
    }
}

// =========================================================
// WIFI CONNECTION
// =========================================================

fn connect_wifi(
    wifi: &mut BlockingWifi<EspWifi<'static>>,
) -> anyhow::Result<()> {
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: WIFI_SSID.try_into().unwrap(),
        password: WIFI_PASS.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;

    wifi.start()?;
    log::info!("WiFi started, connecting to SSID: {}", WIFI_SSID);

    wifi.connect()?;
    log::info!("WiFi connected!");

    wifi.wait_netif_up()?;
    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    log::info!("WiFi DHCP info: IP={}", ip_info.ip);

    Ok(())
}

// =========================================================
// MQTT CLIENT - THINGSBOARD
// =========================================================

/// Bulatkan 2 desimal supaya nilai di ThingsBoard rapi (mis. 30.1, bukan 30.100000381)
fn r2(x: f32) -> f64 {
    ((x as f64) * 100.0).round() / 100.0
}

/// Kirim telemetri ke ThingsBoard dengan key terpisah per sensor (format flat)
fn send_to_thingsboard(client: &mut EspMqttClient<'_>, payload: &TelemetryPayload) {
    let s = &payload.sensors;

    let flat = serde_json::json!({
        // Sensor gas (mV)
        "mq6": r2(s.mq6),
        "mq135": r2(s.mq135),
        "mq3": r2(s.mq3),
        "mq7": r2(s.mq7),
        "tgs2611": r2(s.tgs2611),
        "tgs2602": r2(s.tgs2602),
        "tgs2600": r2(s.tgs2600),
        "tgs2620": r2(s.tgs2620),
        // Suhu & kelembaban
        "dht_temp": r2(s.dht_temp),
        "dht_hum": r2(s.dht_hum),
        // Waktu sejak ESP32 menyala (ms)
        "uptime_ms": payload.timestamp,
        // Penanda versi build (untuk memastikan OTA berhasil)
        "build_tag": BUILD_TAG,
    });

    match serde_json::to_string(&flat) {
        Ok(json) => {
            match client.enqueue(TB_TELEMETRY_TOPIC, QoS::AtLeastOnce, false, json.as_bytes()) {
                Ok(_) => log::info!("ThingsBoard: telemetry sent ({} bytes)", json.len()),
                Err(e) => log::error!("ThingsBoard MQTT publish error: {:?}", e),
            }
        }
        Err(e) => log::error!("JSON serialize error: {:?}", e),
    }
}

/// Publish firmware attributes so ThingsBoard knows current version
fn publish_fw_attributes(client: &mut EspMqttClient<'_>) {
    let attrs = format!(
        r#"{{"current_fw_title":"{}","current_fw_version":"{}"}}"#,
        FW_TITLE, FW_VERSION
    );
    match client.enqueue(TB_ATTRIBUTES_TOPIC, QoS::AtLeastOnce, false, attrs.as_bytes()) {
        Ok(_) => log::info!("ThingsBoard: firmware attributes published"),
        Err(e) => log::error!("ThingsBoard: failed to publish fw attributes: {:?}", e),
    }
}

// =========================================================
// HTTP CLIENT - AZURE IOT HUB
// =========================================================

fn send_to_azure(payload: &TelemetryPayload) {
    let json = match serde_json::to_string(payload) {
        Ok(j) => j,
        Err(e) => {
            log::error!("Azure: JSON error: {:?}", e);
            return;
        }
    };

    let config = HttpConfiguration {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        ..Default::default()
    };

    let connection = match EspHttpConnection::new(&config) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Azure: HTTP connection error: {:?}", e);
            return;
        }
    };

    let mut client = HttpClient::wrap(connection);

    let content_length_str = json.len().to_string();
    let headers = [
        ("Content-Type", "application/json"),
        ("Content-Length", content_length_str.as_str()),
        ("Authorization", AZURE_SAS_TOKEN),
    ];

    match client.request(Method::Post, AZURE_IOT_ENDPOINT, &headers) {
        Ok(mut request) => {
            if let Err(e) = request.write_all(json.as_bytes()) {
                log::error!("Azure: write error: {:?}", e);
                return;
            }
            match request.submit() {
                Ok(response) => {
                    let status = response.status();
                    if status >= 200 && status < 300 {
                        log::info!("Azure: data sent successfully (HTTP {})", status);
                    } else {
                        log::warn!("Azure: unexpected HTTP status {}", status);
                    }
                }
                Err(e) => log::error!("Azure: submit error: {:?}", e),
            }
        }
        Err(e) => log::error!("Azure: request error: {:?}", e),
    }
}

// =========================================================
// HTTP CLIENT - MONGODB ATLAS DATA API
// =========================================================

fn send_to_mongodb(payload: &TelemetryPayload) {
    let doc = match serde_json::to_value(payload) {
        Ok(v) => v,
        Err(e) => {
            log::error!("MongoDB: JSON error: {:?}", e);
            return;
        }
    };

    let body = serde_json::json!({
        "dataSource": "Cluster0",
        "database": MONGO_DATABASE,
        "collection": MONGO_COLLECTION,
        "document": doc
    });

    let json = match serde_json::to_string(&body) {
        Ok(j) => j,
        Err(e) => {
            log::error!("MongoDB: body serialize error: {:?}", e);
            return;
        }
    };

    let config = HttpConfiguration {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        ..Default::default()
    };

    let connection = match EspHttpConnection::new(&config) {
        Ok(c) => c,
        Err(e) => {
            log::error!("MongoDB: HTTP connection error: {:?}", e);
            return;
        }
    };

    let mut client = HttpClient::wrap(connection);

    let content_length_str = json.len().to_string();
    let headers = [
        ("Content-Type", "application/json"),
        ("Content-Length", content_length_str.as_str()),
        ("api-key", MONGO_API_KEY),
        ("Accept", "application/json"),
    ];

    match client.request(Method::Post, MONGO_DATA_API_URL, &headers) {
        Ok(mut request) => {
            if let Err(e) = request.write_all(json.as_bytes()) {
                log::error!("MongoDB: write error: {:?}", e);
                return;
            }
            match request.submit() {
                Ok(response) => {
                    let status = response.status();
                    if status >= 200 && status < 300 {
                        log::info!("MongoDB: data inserted successfully (HTTP {})", status);
                    } else {
                        log::warn!("MongoDB: unexpected HTTP status {}", status);
                    }
                }
                Err(e) => log::error!("MongoDB: submit error: {:?}", e),
            }
        }
        Err(e) => log::error!("MongoDB: request error: {:?}", e),
    }
}

// =========================================================
// OTA - THINGSBOARD MQTT FIRMWARE UPDATE
// =========================================================

/// Pesan masuk dari MQTT (diteruskan dari callback ke main loop lewat channel)
enum MqttIn {
    Connected,
    Msg { topic: String, data: Vec<u8> },
}

/// Info firmware yang ditawarkan ThingsBoard (shared attributes)
struct FwInfo {
    title: String,
    version: String,
    size: usize,
    checksum: String,
    algo: String,
}

fn parse_fw_info(obj: &serde_json::Value) -> Option<FwInfo> {
    let title = obj.get("fw_title")?.as_str()?.to_string();
    let version = obj.get("fw_version")?.as_str()?.to_string();
    let size = obj.get("fw_size")?.as_u64()? as usize;
    let checksum = obj
        .get("fw_checksum")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let algo = obj
        .get("fw_checksum_algorithm")
        .and_then(|v| v.as_str())
        .unwrap_or("SHA256")
        .to_string();
    Some(FwInfo { title, version, size, checksum, algo })
}

fn has_fw_key(obj: &serde_json::Value) -> bool {
    obj.as_object()
        .map_or(false, |o| o.keys().any(|k| k.starts_with("fw_")))
}

fn sha256_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Subscribe ke semua topik yang dibutuhkan ThingsBoard (atribut + OTA)
fn subscribe_tb_topics(client: &mut EspMqttClient<'_>) {
    for topic in [TB_ATTRIBUTES_TOPIC, TB_ATTR_RESPONSE_TOPIC, TB_FW_RESPONSE_TOPIC] {
        if let Err(e) = client.subscribe(topic, QoS::AtLeastOnce) {
            log::error!("MQTT: gagal subscribe {}: {:?}", topic, e);
        }
    }
}

/// Minta info firmware (shared attributes) dari ThingsBoard
fn request_fw_attributes(client: &mut EspMqttClient<'_>) {
    let body = r#"{"sharedKeys":"fw_checksum,fw_checksum_algorithm,fw_size,fw_title,fw_version"}"#;
    match client.enqueue(TB_ATTR_REQUEST_TOPIC, QoS::AtLeastOnce, false, body.as_bytes()) {
        Ok(_) => log::info!("OTA: meminta info firmware ke ThingsBoard"),
        Err(e) => log::error!("OTA: gagal meminta info firmware: {:?}", e),
    }
}

/// Laporkan status OTA ke ThingsBoard (DOWNLOADING, DOWNLOADED, VERIFIED, UPDATING, UPDATED, FAILED)
fn publish_fw_state(client: &mut EspMqttClient<'_>, state: &str, error: Option<&str>) {
    let mut body = serde_json::json!({
        "current_fw_title": FW_TITLE,
        "current_fw_version": FW_VERSION,
        "fw_state": state,
    });
    if let Some(e) = error {
        body["fw_error"] = serde_json::Value::from(e);
    }
    if let Err(e) = client.enqueue(
        TB_TELEMETRY_TOPIC,
        QoS::AtLeastOnce,
        false,
        body.to_string().as_bytes(),
    ) {
        log::error!("OTA: gagal kirim fw_state {}: {:?}", state, e);
    } else {
        log::info!("OTA: fw_state = {}", state);
    }
}

/// Unduh firmware per potongan, tulis ke partisi OTA, verifikasi SHA-256,
/// lalu restart. Fungsi ini TIDAK kembali jika berhasil (perangkat restart).
fn run_ota(
    client: &mut EspMqttClient<'_>,
    ota: &mut EspOta,
    rx: &mpsc::Receiver<MqttIn>,
    info: &FwInfo,
    nvs: &mut EspNvs<NvsDefault>,
) -> Result<(), String> {
    if !info.algo.eq_ignore_ascii_case("SHA256") && !info.algo.eq_ignore_ascii_case("SHA-256") {
        return Err(format!(
            "checksum {} tidak didukung, upload paket dengan SHA-256",
            info.algo
        ));
    }
    if info.checksum.len() != 64 {
        return Err("fw_checksum tidak valid".to_string());
    }
    if info.size == 0 || info.size > OTA_MAX_SIZE {
        return Err(format!(
            "fw_size {} di luar batas (maks {} bytes)",
            info.size, OTA_MAX_SIZE
        ));
    }

    log::info!(
        "OTA: mengunduh {} v{} ({} bytes)",
        info.title, info.version, info.size
    );
    publish_fw_state(client, "DOWNLOADING", None);

    let mut update = ota
        .initiate_update()
        .map_err(|e| format!("initiate_update gagal: {:?}", e))?;

    let mut hasher = Sha256::new();
    let request_id = (millis() % 1_000_000) as u32 + 1;
    let mut chunk_idx: u32 = 0;
    let mut received: usize = 0;

    // Buang pesan lama di antrean
    while rx.try_recv().is_ok() {}

    loop {
        let req_topic = format!("{}{}/chunk/{}", TB_FW_REQUEST_TOPIC_PREFIX, request_id, chunk_idx);
        let resp_topic = format!("v2/fw/response/{}/chunk/{}", request_id, chunk_idx);
        let mut attempts = 0u32;

        let chunk: Vec<u8> = 'wait: loop {
            client
                .enqueue(
                    &req_topic,
                    QoS::AtLeastOnce,
                    false,
                    OTA_CHUNK_SIZE.to_string().as_bytes(),
                )
                .map_err(|e| format!("gagal meminta chunk {}: {:?}", chunk_idx, e))?;

            let deadline = millis() + 15_000;
            while millis() < deadline {
                match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(MqttIn::Msg { topic, data }) => {
                        if topic == resp_topic {
                            break 'wait data;
                        }
                    }
                    Ok(MqttIn::Connected) => {
                        log::warn!("OTA: MQTT tersambung ulang, subscribe ulang dan minta chunk lagi");
                        subscribe_tb_topics(client);
                        continue 'wait;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("channel MQTT terputus".to_string());
                    }
                }
            }

            attempts += 1;
            if attempts >= 3 {
                return Err(format!("timeout menunggu chunk {}", chunk_idx));
            }
            log::warn!("OTA: timeout chunk {}, coba lagi ({}/3)", chunk_idx, attempts);
        };

        // Chunk kosong = data sudah habis
        if chunk.is_empty() {
            break;
        }
        if received + chunk.len() > info.size {
            return Err("data yang diterima melebihi fw_size".to_string());
        }

        update
            .write(&chunk)
            .map_err(|e| format!("tulis flash gagal: {:?}", e))?;
        hasher.update(&chunk);
        received += chunk.len();
        chunk_idx += 1;

        if chunk_idx % 10 == 0 || received >= info.size {
            log::info!(
                "OTA: {}/{} bytes ({}%)",
                received,
                info.size,
                received * 100 / info.size
            );
        }
        if received >= info.size {
            break;
        }
    }

    if received != info.size {
        return Err(format!(
            "ukuran diterima {} tidak sama dengan fw_size {}",
            received, info.size
        ));
    }
    publish_fw_state(client, "DOWNLOADED", None);

    let digest = hasher.finalize();
    let hex = sha256_hex(digest.as_slice());
    if !hex.eq_ignore_ascii_case(&info.checksum) {
        let calc: String = hex.chars().take(8).collect();
        let want: String = info.checksum.chars().take(8).collect();
        return Err(format!("checksum tidak cocok (hitung {}..., server {}...)", calc, want));
    }
    log::info!("OTA: checksum SHA-256 cocok");
    publish_fw_state(client, "VERIFIED", None);

    publish_fw_state(client, "UPDATING", None);

    // Catat versi yang sedang dipasang. Jika setelah restart versi yang berjalan
    // tetap tidak sama dengan paket, OTA tidak diulang terus-menerus (anti-loop).
    let _ = nvs.set_str("tried_ver", &info.version);
    update.complete().map_err(|e| {
        let _ = nvs.remove("tried_ver");
        format!("validasi image gagal: {:?}", e)
    })?;

    log::info!("OTA: firmware v{} terpasang, restart...", info.version);
    FreeRtos::delay_ms(1500);
    esp_idf_svc::hal::reset::restart();
}

// =========================================================
// ENTRY POINT
// =========================================================

fn main() -> anyhow::Result<()> {
    // Initialize ESP-IDF system (logger, allocator, etc.)
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    // Jalankan program utama di thread dengan stack besar, supaya tidak
    // bergantung pada CONFIG_ESP_MAIN_TASK_STACK_SIZE (mencegah stack overflow).
    std::thread::Builder::new()
        .name("enose".into())
        .stack_size(32 * 1024)
        .spawn(run)?
        .join()
        .map_err(|_| anyhow::anyhow!("thread utama panic"))?
}

fn run() -> anyhow::Result<()> {
    BOOT_MS.store(millis() as u32, Ordering::Relaxed);

    log::info!("=========================================");
    log::info!("  Electronic Nose v{}  [{}]", FW_VERSION, BUILD_TAG);
    log::info!("  >>> FIRMWARE BARU v{} (hasil OTA) <<<", FW_VERSION);
    log::info!("  ESP32-S3 - WiFi/MQTT/HTTP/OTA");
    log::info!("=========================================");

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // ---------------------------------------------------------
    // 1. UART0 untuk komunikasi serial GUI (COM8)
    // ---------------------------------------------------------
    let uart_config = UartConfig::default();
    let uart = UartDriver::new(
        peripherals.uart0,
        peripherals.pins.gpio43,  // TX
        peripherals.pins.gpio44,  // RX
        Option::<esp_idf_svc::hal::gpio::AnyIOPin>::None,  // CTS
        Option::<esp_idf_svc::hal::gpio::AnyIOPin>::None,  // RTS
        &uart_config,
    )?;

    // ---------------------------------------------------------
    // 2. I2C untuk ADS1115 (SDA=GPIO8, SCL=GPIO9)
    // ---------------------------------------------------------
    let i2c_config = I2cConfig::new().baudrate(Hertz(400_000));
    let mut i2c = I2cDriver::new(
        peripherals.i2c0,
        peripherals.pins.gpio8,   // SDA
        peripherals.pins.gpio9,   // SCL
        &i2c_config,
    )?;

    // Scan I2C: tampilkan semua perangkat yang menjawab (cek wiring ADS1115)
    log::info!("Scanning I2C bus (SDA=GPIO8, SCL=GPIO9)...");
    let mut i2c_found = 0;
    for addr in 1u8..127 {
        let mut b = [0u8; 1];
        if i2c.read(addr, &mut b, 50).is_ok() {
            log::info!("I2C device found at 0x{:02X}", addr);
            i2c_found += 1;
        }
    }
    if i2c_found == 0 {
        log::warn!("Tidak ada perangkat I2C terdeteksi! Cek daya, SDA/SCL, dan pull-up.");
    }

    // ---------------------------------------------------------
    // 5. DHT22 Sensor Pin (GPIO10, Open Drain + Pull-Up)
    // ---------------------------------------------------------
    let mut dht_pin = PinDriver::input_output_od(peripherals.pins.gpio10)?;
    dht_pin.set_pull(Pull::Up)?;
    dht_pin.set_high()?;

    // ---------------------------------------------------------
    // 6. WiFi Connection
    // ---------------------------------------------------------
    log::info!("Initializing WiFi...");
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs.clone()))?,
        sysloop.clone(),
    )?;

    let wifi_connected = match connect_wifi(&mut wifi) {
        Ok(()) => {
            log::info!("WiFi connected successfully!");
            true
        }
        Err(e) => {
            log::error!("WiFi connection failed: {:?}", e);
            log::warn!("Continuing without network - sensor reading & UART still work");
            false
        }
    };

    // ---------------------------------------------------------
    // 7. MQTT Client - ThingsBoard
    // ---------------------------------------------------------
    let mqtt_config = MqttClientConfiguration {
        client_id: Some("esp32_enose"),
        username: Some(TB_ACCESS_TOKEN),
        buffer_size: 8192,      // cukup untuk satu chunk OTA (4096 byte)
        out_buffer_size: 1024,
        ..Default::default()
    };

    let broker_url = format!("mqtt://{}:{}", TB_MQTT_HOST, TB_MQTT_PORT);

    // Pesan MQTT diteruskan ke main loop lewat channel
    let (mqtt_tx, mqtt_rx) = mpsc::channel::<MqttIn>();

    let mut mqtt_client: Option<EspMqttClient<'_>> = if wifi_connected {
        log::info!("Connecting to ThingsBoard MQTT at {}...", broker_url);

        // Penyusun pesan besar yang datang terpotong-potong
        let mut asm_topic = String::new();
        let mut asm_data: Vec<u8> = Vec::new();

        match EspMqttClient::new_cb(&broker_url, &mqtt_config, move |event| {
            match event.payload() {
                EventPayload::Connected(_) => {
                    let _ = mqtt_tx.send(MqttIn::Connected);
                }
                EventPayload::Received { topic, data, details, .. } => match details {
                    Details::Complete => {
                        let _ = mqtt_tx.send(MqttIn::Msg {
                            topic: topic.unwrap_or("").to_string(),
                            data: data.to_vec(),
                        });
                    }
                    Details::InitialChunk(info) => {
                        asm_topic = topic.unwrap_or("").to_string();
                        asm_data.clear();
                        asm_data.reserve(info.total_data_size);
                        asm_data.extend_from_slice(data);
                    }
                    Details::SubsequentChunk(info) => {
                        asm_data.extend_from_slice(data);
                        if asm_data.len() >= info.total_data_size {
                            let _ = mqtt_tx.send(MqttIn::Msg {
                                topic: std::mem::take(&mut asm_topic),
                                data: std::mem::take(&mut asm_data),
                            });
                        }
                    }
                },
                _ => {}
            }
        }) {
            Ok(client) => {
                log::info!("MQTT client started (menunggu koneksi ke ThingsBoard)");
                Some(client)
            }
            Err(e) => {
                log::error!("MQTT connection failed: {:?}", e);
                None
            }
        }
    } else {
        None
    };

    // ---------------------------------------------------------
    // 9. Main Loop
    // ---------------------------------------------------------
    let mut state = SystemState::new();

    // OTA
    let mut ota = EspOta::new()?;
    let mut ota_nvs = EspNvs::new(nvs.clone(), "ota", true)?;
    let mut last_heartbeat_ms: u64 = 0;
    let mut pending_fw: Option<FwInfo> = None;
    let mut boot_reported = false;
    let mut slot_validated = false;
    let mut cmd_buf = [0u8; 64];
    let mut cmd_len: usize = 0;

    // Counter for throttling cloud sends (not every second)
    let mut cloud_send_counter: u32 = 0;

    // DHT22 hanya boleh dibaca maksimal 1x tiap 2 detik
    let mut last_dht_ms: u64 = 0;
    const CLOUD_SEND_INTERVAL: u32 = 5; // Send to cloud every 5 seconds

    log::info!(">>> System ready! Entering main loop...");

    loop {
        let now = millis();

        // =====================================================
        // BACA COMMAND DARI UART0 (NON-BLOCKING)
        // =====================================================
        let mut rx_buf = [0u8; 1];
        while uart.read(&mut rx_buf, 0).unwrap_or(0) == 1 {
            let b = rx_buf[0];
            if b == b'\n' || b == b'\r' {
                if cmd_len > 0 {
                    if let Ok(cmd_str) = core::str::from_utf8(&cmd_buf[..cmd_len]) {
                        let command = cmd_str.trim();

                        // Perintah manual: minta ulang info firmware OTA
                        if command == "OTA" || command == "ota" {
                            log::info!(">>> Manual OTA check requested");
                            // Perintah manual: hapus catatan anti-loop agar OTA boleh dicoba lagi
                            let _ = ota_nvs.remove("tried_ver");
                            if let Some(ref mut client) = mqtt_client {
                                request_fw_attributes(client);
                            }
                        }
                    }
                    cmd_len = 0;
                }
            } else if cmd_len < cmd_buf.len() {
                cmd_buf[cmd_len] = b;
                cmd_len += 1;
            }
        }

        // =====================================================
        // MQTT MASUK: koneksi & atribut ThingsBoard (untuk OTA)
        // =====================================================
        while let Ok(incoming) = mqtt_rx.try_recv() {
            let Some(ref mut client) = mqtt_client else {
                continue;
            };
            match incoming {
                MqttIn::Connected => {
                    log::info!("MQTT: terhubung ke ThingsBoard, subscribe topik OTA");
                    subscribe_tb_topics(client);
                    publish_fw_attributes(client);
                    if !boot_reported {
                        publish_fw_state(client, "UPDATED", None);
                        boot_reported = true;
                    }
                    request_fw_attributes(client);
                }
                MqttIn::Msg { topic, data } => {
                    let is_attr = topic == TB_ATTRIBUTES_TOPIC;
                    let is_attr_resp = topic.starts_with("v1/devices/me/attributes/response/");
                    if !(is_attr || is_attr_resp) {
                        continue;
                    }
                    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&data) else {
                        continue;
                    };
                    let obj = v.get("shared").unwrap_or(&v);
                    match parse_fw_info(obj) {
                        Some(info) => {
                            if info.title != FW_TITLE {
                                log::warn!(
                                    "OTA diabaikan: fw_title '{}' tidak sama dengan FW_TITLE '{}'",
                                    info.title, FW_TITLE
                                );
                                publish_fw_state(
                                    client,
                                    "FAILED",
                                    Some("fw_title tidak cocok dengan firmware di perangkat"),
                                );
                            } else if info.version == FW_VERSION {
                                log::info!("OTA: firmware sudah versi terbaru (v{})", FW_VERSION);
                                let _ = ota_nvs.remove("tried_ver");
                            } else {
                                let mut nbuf = [0u8; 32];
                                let tried = ota_nvs
                                    .get_str("tried_ver", &mut nbuf)
                                    .ok()
                                    .flatten()
                                    .map(|t| t.to_string());
                                if tried.as_deref() == Some(info.version.as_str()) {
                                    log::warn!(
                                        "OTA dilewati: v{} sudah pernah dipasang tetapi firmware yang berjalan v{}. Pastikan FW_VERSION di binary sama dengan versi paket, lalu kirim perintah OTA untuk mencoba lagi.",
                                        info.version, FW_VERSION
                                    );
                                    publish_fw_state(
                                        client,
                                        "FAILED",
                                        Some("versi di binary tidak sama dengan versi paket"),
                                    );
                                } else {
                                    log::info!(
                                        "OTA: firmware baru tersedia v{} -> v{} ({} bytes)",
                                        FW_VERSION, info.version, info.size
                                    );
                                    pending_fw = Some(info);
                                }
                            }
                        }
                        None => {
                            // Update parsial dari server: minta atribut lengkap
                            if is_attr && has_fw_key(obj) {
                                request_fw_attributes(client);
                            }
                        }
                    }
                }
            }
        }

        // Mulai OTA jika ada firmware baru
        if pending_fw.is_some() {
            if let Some(ref mut client) = mqtt_client {
                if let Some(info) = pending_fw.take() {
                    log::info!(">>> OTA dimulai: v{} -> v{}", FW_VERSION, info.version);

                    if let Err(e) = run_ota(client, &mut ota, &mqtt_rx, &info, &mut ota_nvs) {
                        log::error!("OTA gagal: {}", e);
                        publish_fw_state(client, "FAILED", Some(e.as_str()));
                    }
                }
            }
        }

        // Heartbeat tiap 10 detik: menunjukkan versi firmware yang sedang berjalan
        if uptime_ms().saturating_sub(last_heartbeat_ms) >= 10_000 {
            last_heartbeat_ms = uptime_ms();
            let free_heap = unsafe { esp_idf_svc::sys::esp_get_free_heap_size() };
            log::info!(
                "[HEARTBEAT] v{} ({}) | uptime {} detik | heap bebas {} KB",
                FW_VERSION, BUILD_TAG, uptime_ms() / 1000, free_heap / 1024
            );
        }

        // Konfirmasi firmware valid setelah berjalan stabil 20 detik (rollback protection)
        if !slot_validated && uptime_ms() > 20_000 {
            slot_validated = true;
            match ota.mark_running_slot_valid() {
                Ok(()) => log::info!("OTA: firmware berjalan stabil, ditandai valid"),
                Err(e) => log::debug!("OTA: mark valid dilewati: {:?}", e),
            }
        }

        // =====================================================
        // BACA SENSOR SETIAP 1 DETIK
        // =====================================================
        if now.saturating_sub(state.last_sensor_read_ms) >= 1000 {
            // Baca DHT22 (tiap 2 detik; di antaranya pakai nilai terakhir)
            if now.saturating_sub(last_dht_ms) >= 2000 {
                if let Some((temp, hum)) = read_dht22(&mut dht_pin) {
                    state.sensors.dht_temp = temp;
                    state.sensors.dht_hum = hum;
                }
                last_dht_ms = now;
            }

            // Baca Gas Sensors via ADS1115
            read_gas_sensors(&mut state, &mut i2c);

            // Build telemetry payload
            let payload = build_telemetry(&state);

            // Tampilkan data sensor terpisah di serial monitor
            log_readable(&state.sensors);

            // JSON lengkap hanya tampil jika level log = debug
            if let Ok(json) = serde_json::to_string(&payload) {
                log::debug!("{}", json);
            }

            // Kirim ke cloud services (setiap CLOUD_SEND_INTERVAL detik)
            cloud_send_counter += 1;
            if cloud_send_counter >= CLOUD_SEND_INTERVAL && wifi_connected {
                cloud_send_counter = 0;

                // ThingsBoard via MQTT
                if let Some(ref mut client) = mqtt_client {
                    send_to_thingsboard(client, &payload);
                }

                // Azure IoT Hub via HTTPS
                if ENABLE_AZURE {
                    send_to_azure(&payload);
                }

                // MongoDB Atlas via HTTPS
                if ENABLE_MONGODB {
                    send_to_mongodb(&payload);
                }
            }

            state.last_sensor_read_ms = now;
        }

        FreeRtos::delay_ms(1);
    }
}