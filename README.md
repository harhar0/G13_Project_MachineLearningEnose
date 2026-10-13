# Electronic Nose (E-nose) — ESP32-S3 + Rust

Firmware hidung elektronik berbasis **ESP32-S3** yang ditulis dengan **Rust** (`std`, `esp-idf-svc`).
Membaca sensor gas dan suhu/kelembapan, mengirim data ke **ThingsBoard** lewat MQTT, dan bisa
diperbarui dari jarak jauh lewat **OTA (Over-The-Air)**.

## Fitur

- Membaca **DHT22** (suhu dan kelembapan).
- Membaca **8 sensor gas** lewat 2 modul **ADS1115** (I2C): MQ-6, MQ-135, MQ-3, MQ-7, TGS2611, TGS2602, TGS2600, TGS2620.
- Mengirim telemetri ke **ThingsBoard** (MQTT), satu key per sensor.
- **OTA update** dari menu *OTA updates* ThingsBoard, dengan verifikasi checksum SHA-256.
- Perlindungan OTA: rollback bawaan ESP-IDF dan pengaman anti unduh berulang.
- Log serial yang mudah dibaca, ditambah *heartbeat* tiap 10 detik.

> Kode untuk Azure IoT Hub dan MongoDB Atlas masih ada tetapi **dimatikan** (`ENABLE_AZURE` dan `ENABLE_MONGODB` = `false`).
> Lihat bagian [Penyimpanan ke MongoDB](#penyimpanan-ke-mongodb).

## Status

| Bagian | Status |
|---|---|
| DHT22 (GPIO10) | Berjalan |
| WiFi dan MQTT ke ThingsBoard | Berjalan |
| OTA lewat ThingsBoard | Berjalan (teruji sampai versi 0.8.0) |
| Sensor gas via ADS1115 | Belum diuji: modul ADS1115 belum terpasang, bacaan tetap 0.0 |
| Penyimpanan ke MongoDB | Belum terintegrasi di firmware |

## Perangkat keras

| Komponen | Koneksi |
|---|---|
| ESP32-S3 (flash 16 MB) | Board utama |
| DHT22 | Data di **GPIO10** (pull-up 4.7k–10k ke 3.3V jika modul tanpa resistor) |
| ADS1115 #1 (alamat `0x48`) | MQ-6 (ch0), MQ-135 (ch1), MQ-3 (ch2), MQ-7 (ch3) |
| ADS1115 #2 (alamat `0x49`) | TGS2611 (ch0), TGS2602 (ch1), TGS2600 (ch2), TGS2620 (ch3) |
| I2C | SDA = **GPIO8**, SCL = **GPIO9**, kecepatan 400 kHz |
| UART0 | TX = GPIO43, RX = GPIO44 (perintah teks) |

Catatan wiring ADS1115:

- Alamat `0x48`: pin ADDR ke GND. Alamat `0x49`: pin ADDR ke VDD.
- Pasok VDD **3.3 V**. Jangan beri tegangan di atas VDD + 0.3 V ke pin input.
- Keluaran sensor MQ/TGS yang dipasok 5 V perlu **pembagi tegangan** sebelum masuk ke ADS1115.

## Struktur project

```
.
├── .cargo/config.toml     # target, linker, runner espflash, versi ESP-IDF
├── src/main.rs            # seluruh kode firmware
├── partitions.csv         # tabel partisi dengan dua slot OTA
├── sdkconfig.defaults     # konfigurasi ESP-IDF
├── build.rs               # skrip build (embuild)
├── rust-toolchain.toml    # toolchain Rust untuk Xtensa (channel "esp")
└── Cargo.toml
```

Partisi flash: `nvs`, `otadata`, `phy_init`, `ota_0` (1800K), `ota_1` (1800K).
Ukuran firmware tidak boleh melebihi 1800 KB.

## Persiapan lingkungan (Windows)

1. Pasang toolchain Rust untuk ESP32: [`espup`](https://github.com/esp-rs/espup), lalu jalankan `espup install`.
2. Pasang alat bantu:

   ```powershell
   cargo install ldproxy espflash
   ```

3. Pasang **Git** dan **Python 3** (dibutuhkan saat ESP-IDF diunduh otomatis).
4. **Letakkan project di folder dengan path pendek**, misalnya `C:\enose`.
   Path panjang membuat build ESP-IDF gagal dengan pesan `Too long output directory`.

## Konfigurasi

Edit konstanta di bagian atas `src/main.rs`:

| Konstanta | Fungsi |
|---|---|
| `WIFI_SSID`, `WIFI_PASS` | WiFi yang dipakai |
| `TB_MQTT_HOST`, `TB_MQTT_PORT` | Server ThingsBoard (default `thingsboard.cloud:1883`) |
| `TB_ACCESS_TOKEN` | Access token device di ThingsBoard |
| `FW_TITLE` | Judul firmware, harus sama dengan Title paket OTA |
| `FW_VERSION` | Versi firmware, harus sama dengan Version paket OTA |
| `BUILD_TAG` | Penanda build, tampil di log dan telemetri |

> **Jangan menyebarkan** password WiFi dan access token (misalnya mengunggah `main.rs` ke repository publik).
> Semuanya tertanam di firmware.

## Build dan flash

Sambungkan ESP32-S3 lewat USB, lalu:

```powershell
cd C:\enose
cargo run --release
```

Perintah ini membangun, mem-flash, dan membuka serial monitor. Runner di `.cargo/config.toml`
memasang `partitions.csv` saat flash, jadi tabel partisi OTA ikut terpasang.
Build ESP-IDF pertama kali memakan waktu lama.

Jika tidak mendeteksi port otomatis: `cargo run --release -- --port COM5`.
Jika gagal terhubung: tahan tombol **BOOT**, tekan sebentar **RESET**, lepas BOOT, lalu ulangi.

## Data di ThingsBoard

Telemetri dikirim tiap 5 detik (`CLOUD_SEND_INTERVAL`), dengan key terpisah per sensor:

| Key | Isi |
|---|---|
| `mq6`, `mq135`, `mq3`, `mq7` | Tegangan sensor MQ (mV) |
| `tgs2611`, `tgs2602`, `tgs2600`, `tgs2620` | Tegangan sensor TGS (mV) |
| `dht_temp`, `dht_hum` | Suhu (°C) dan kelembapan (%) |
| `uptime_ms` | Lama perangkat menyala (ms) |
| `build_tag` | Penanda build |
| `current_fw_title`, `current_fw_version`, `fw_state`, `fw_error` | Status OTA |

DHT22 dibaca tiap 2 detik, sensor gas tiap 1 detik.

## Perintah serial (UART0)

| Perintah | Fungsi |
|---|---|
| `OTA` | Meminta ulang info firmware dari ThingsBoard dan menghapus catatan anti-loop |

## OTA update lewat ThingsBoard

1. **Naikkan versi** di `src/main.rs`, misalnya:

   ```rust
   const FW_VERSION: &str = "0.9.0";
   const BUILD_TAG: &str = "ota-test-0.9.0";
   ```

2. **Build dan buat file `.bin`.** Nama berkas hasil build sama dengan nama package di `Cargo.toml`
   (contoh di bawah memakai `Enose`):

   ```powershell
   cargo build --release
   del firmware.bin
   & "$env:USERPROFILE\.espressif\python_env\idf5.3_py3.11_env\Scripts\python.exe" -m esptool --chip esp32s3 elf2image --flash_mode dio --flash_freq 40m --flash_size 16MB -o firmware.bin target\xtensa-esp32s3-espidf\release\Enose
   ```

3. **Pastikan isinya benar** (harus `True`):

   ```powershell
   & "$env:USERPROFILE\.espressif\python_env\idf5.3_py3.11_env\Scripts\python.exe" -c "d=open('firmware.bin','rb').read(); print(b'ota-test-0.9.0' in d)"
   ```

4. **Upload paket** di ThingsBoard: *Advanced features → OTA updates → tambah paket*.
   - Title: `esp32_enose` (sama dengan `FW_TITLE`)
   - Version: `0.9.0` (sama dengan `FW_VERSION`)
   - Package type: Firmware, Checksum algorithm: **SHA-256**
   - File: `firmware.bin`
5. **Assign** paket: *Entities → Devices →* pilih device *→ Edit → Firmware*.
6. **Pantau** di serial monitor dan di tab *Attributes* device (`fw_state`).
   Setelah selesai, perangkat restart sendiri dan banner menampilkan versi baru.

Urutan `fw_state`: `DOWNLOADING` → `DOWNLOADED` → `VERIFIED` → `UPDATING` → `UPDATED` (atau `FAILED` beserta `fw_error`).

Catatan penting:

- Firmware dasar di board harus sudah memuat kode OTA (di-flash lewat kabel sekali).
- **Title dan Version harus sama persis** dengan `FW_TITLE` dan `FW_VERSION` di binary yang diupload.
  Jika tidak, update ditolak atau berulang.
- Firmware baru ditandai valid setelah berjalan stabil 20 detik. Jika crash sebelum itu,
  bootloader dapat mengembalikan ke versi lama (rollback).

## Penyimpanan ke MongoDB

Fungsi `send_to_mongodb` di `main.rs` memakai endpoint **MongoDB Atlas Data API** dan tidak terhubung ke
database Azure Cosmos DB. Selama `ENABLE_MONGODB = false`, fungsi itu tidak pernah dipanggil.
Menyambungkan ESP32 langsung ke Cosmos DB (protokol MongoDB asli, TLS port 10255) memerlukan
implementasi BSON, `OP_MSG`, dan autentikasi SCRAM sendiri. Alternatif yang lebih sederhana dan aman:
sebuah program terpisah di komputer membaca data dari ThingsBoard lalu menulisnya ke Mongo,
sehingga kredensial database tidak ikut tertanam di firmware.

## Pemecahan masalah

| Gejala | Penyebab dan solusi |
|---|---|
| `Too long output directory` | Path project terlalu panjang. Pindahkan ke path pendek seperti `C:\enose`. |
| `partitions.csv ... not found` | Jangan set `CONFIG_PARTITION_TABLE_CUSTOM` di `sdkconfig.defaults`. Tabel dipasang lewat runner `--partition-table partitions.csv`. |
| Perubahan `sdkconfig.defaults` tidak berlaku | Jalankan `cargo clean --release -p esp-idf-sys`, lalu build ulang. |
| `A stack overflow in task main` | Pastikan `CONFIG_ESP_MAIN_TASK_STACK_SIZE=16384` berlaku (lihat baris di atas) dan firmware berjalan di thread berstack besar. |
| `Tidak ada perangkat I2C terdeteksi` | ADS1115 belum terpasang, atau cek daya 3.3 V, SDA/SCL (GPIO8/GPIO9), alamat, dan pull-up. |
| `ADS ... error` di log | ADS1115 tidak merespons. Penyebabnya sama dengan di atas. |
| `DHT22 read error: Timeout` | Cek pull-up dan kabel data. DHT22 maksimal dibaca sekali per 2 detik. |
| OTA mengunduh berulang | Versi di dalam `firmware.bin` tidak sama dengan versi paket. Samakan `FW_VERSION`, build ulang, dan cek isi `.bin`. |
| OTA `diabaikan: fw_title ...` | Title paket di ThingsBoard tidak sama dengan `FW_TITLE`. |
| `.bin` tidak berubah setelah build | Nama berkas hasil build mengikuti nama package di `Cargo.toml`. Pastikan memakai nama yang benar. |

## Lisensi

Project kuliah. Tentukan lisensi sesuai kebutuhan.
