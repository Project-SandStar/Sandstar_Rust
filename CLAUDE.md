# Sandstar (Rust)
- Kalite kapısı: `bash tools/gates.sh` (fmt, clippy `-D warnings`, `test --no-fail-fast`, `linux-hal,svm` check; ayrıntı `workflow.md`)
- ARM: `cargo arm-build-svm && cargo arm-deb --no-strip` (önce `C:\czb\arm-env.*`; linker yolu `C:\czb` makineye özgü)
- Demo: `cargo run -p sandstar-server` · Sim: `cargo sim-run`

## Kurallar
- Engine yalnız main loop'a aittir; erişim `EngineHandle` (mpsc+oneshot) ile. `Arc<Mutex<Engine>>` ekleme.
- Release `panic="abort"`: ağ/REST girdisi yolunda `unwrap`/dilimleme/`clamp` kullanma; hata döndür.
- Çıkışa giden her değer `is_finite` kontrolünden geçmeli.
- Yeni rota eklerken auth katmanını açıkça belirle (bkz. docs/DevelopingMds/06 R-2).
- Ayrıntılı kurallar `.claude/rules/` altında, dosya yoluna göre yüklenir: panic-safety, numeric-output, engine-async, routes-auth, wire-protocols, svm-unsafe, workflow. `ecc-rust/` genel ECC kurallarıdır; çelişkide proje kuralları geçerlidir.
- Durum ve kararlar: `docs/DevelopingMds/` (00 indeks, 10 kaldığımız yer); faz planları `docs/Progress/`.

## Bilinçli kararlar: "iyileştirme" diye bozma
Değiştirmek gerekirse önce kullanıcıyla konuş. Gerekçe parantezdeki kaynakta.
- **MSRV ve clippy:**
  - `rust-version = "1.82"`.
  - `native_inet.rs` `chunks_exact_to_as_chunks` ve `async_driver.rs` `double_must_use` için gerekçeli `#[allow]` kullanılır (S-5).
  - `as_chunks` 1.88 ister; MSRV'yi yalnız bunun için yükseltme.
- **Toolchain:**
  - `rust-toolchain.toml` 1.99.1'e sabit ve CI ile birlikte değişir.
  - ARM CI job'u `components: rustfmt, clippy` ister (S-5, `cbcfcd8`).
- **ARM build:** `.cargo/config.toml`'da `--sysroot` yok; `C:\czb\arm-env.*` ile `CC`/`AR` verilir (R-21, S-5).
- **Günlük budaması:**
  - Günlük dosyaları `logging.rs` `PruningWriter` ile **dosya adındaki tarihe göre** budanır.
  - `tracing-appender` `max_log_files` kullanılmaz: cihazın dosya sisteminde oluşturma zamanı yok ve kütüphane bu durumda hiçbir dosyayı silmiyor (R-41, `3f1077b`).
- **PWM:** polarity ve diğer sysfs yazmaları C `io_write_cached` ile aynı semantikte. Test bunu `starts_with` ile kontrol eder; üretim yazma yolunu "düzeltme" (S-5, `6535421`).
- **Watchdog (GPIO60):**
  - `initialize.sh` export'u dener ve sonucu journal'a yazar; servis başlangıcını asla başarısız yapmaz.
  - Engine açılışta GPIO60 yoksa ~5 dk yeniden dener; toggle hatasından sonra denemez (R-42, S-8).
- **Dal ve commit:** her S adımı kendi dalında, doğrudan `master`'a commit yok. Commit özeti İngilizce (`workflow.md`, karar 2026-11-18).
- **Sır dosyaları:** sır içeren `tools/` betikleri git dışında (`.gitignore` credentials bloğu) ve hook ile dosya araçlarına kapalı (S-6, S-9).
