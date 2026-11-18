# Sandstar (Rust)
- Test: `cargo test --workspace` · Kapı: `cargo fmt --all -- --check` + `cargo clippy --workspace -- -D warnings`
- ARM: `cargo arm-build-svm && cargo arm-deb --no-strip` (önce `C:\czb\arm-env.*`; linker yolu `C:\czb` makineye özgü)
- Demo: `cargo run -p sandstar-server` · Sim: `cargo sim-run`

## Kurallar
- Engine yalnız main loop'a aittir; erişim `EngineHandle` (mpsc+oneshot) ile. `Arc<Mutex<Engine>>` ekleme.
- Release `panic="abort"`: ağ/REST girdisi yolunda `unwrap`/dilimleme/`clamp` kullanma; hata döndür.
- Çıkışa giden her değer `is_finite` kontrolünden geçmeli.
- Yeni rota eklerken auth katmanını açıkça belirle (bkz. docs/DevelopingMds/06 R-2).
- Ayrıntılı kurallar `.claude/rules/` altında, dosya yoluna göre yüklenir: panic-safety, numeric-output, engine-async, routes-auth, wire-protocols, svm-unsafe, workflow. `ecc-rust/` genel ECC kurallarıdır; çelişkide proje kuralları geçerlidir.
- Durum ve kararlar: `docs/DevelopingMds/` (00 indeks, 10 kaldığımız yer); faz planları `docs/Progress/`.
