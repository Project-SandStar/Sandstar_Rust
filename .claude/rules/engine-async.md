---
paths:
  - "crates/sandstar-server/src/**/*.rs"
  - "crates/sandstar-engine/**/*.rs"
---
# Engine sahipliği ve async

## Engine actor
- `Engine`'in tek sahibi main loop'tur. Diğer görevler ona yalnız `EngineHandle` (mpsc + oneshot) üzerinden erişir.
- `Arc<Mutex<Engine>>` veya benzeri bir paylaşımlı erişim **eklenmez**.
- Yeni bir engine işlemi = yeni bir komut varyantı + `cmd_handler` dalı. Cevap oneshot ile döner. Bekleyen tarafta zaman aşımı olmalı; mevcut eksik R-28 (`crates/sandstar-server/src/rest/mod.rs:160`).

## Runtime: `current_thread`
`#[tokio::main(flavor = "current_thread")]` (`crates/sandstar-server/src/main.rs:90`). **Tek iş parçacığı var: bir görevi bloklamak REST'i, SOX'u, sürücüleri ve poll döngüsünü birlikte durdurur.**
- Bloklayan I/O (sysfs/I2C/UART, dosya, senkron IPC), uzun CPU işleri ve SVM çağrıları `tokio::task::spawn_blocking` içinde çalışır. Async kodda `std::fs` ve `std::thread::sleep` kullanılmaz.
- `std::sync::Mutex`/`RwLock` guard'ı `.await` boyunca tutulmaz. Kısa kritik bölüm → guard'ı düşür → sonra await.
- Kanallar: sınırsız (`unbounded`) kanal yeni kodda kullanılmaz. Kapasiteyi ve dolu durumdaki davranışı (bekle / düşür / hata) açıkça seç.
- Yeni arka plan görevinin kapanış yolu olmalı. Main loop SIGINT, SIGTERM (`signal::TermSignal`, S-43) ve IPC `Shutdown` ile döngüden çıkar ve kapanış adımlarını koşar.

## Sürücüler (driver actor)
- Sürücü hatası tüm actor'ü düşürmez; yeniden bağlanma ve backoff sürücünün içinde kalır (R-25).
