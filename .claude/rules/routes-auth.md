---
paths:
  - "crates/sandstar-server/src/rest/**/*.rs"
  - "crates/sandstar-server/src/roxwarp/**/*.rs"
  - "crates/sandstar-server/src/main.rs"
  - "crates/sandstar-server/src/sox/sox_handlers.rs"
  - "crates/sandstar-server/src/alerts.rs"
---
# Rotalar ve kimlik doğrulama

Bugünkü durum: üretim servisi `0.0.0.0` üzerinde ve auth'suz çalışıyor (R-1). Birçok yazma rotası auth katmanından **sonra** merge edildiği için korumasız (R-2). Auth modeli henüz karara bağlanmadı (Q-4, S-3). Bu kurallar durumu daha da kötüleştirmemek içindir.

## Yeni veya değişen rota
- Rotanın auth katmanının **içinde mi dışında mı** olduğu açıkça belirlenir ve commit mesajında yazılır. axum'da `.layer(...)` yalnız kendisinden önce eklenen rotaları sarar. Layer'dan sonra yapılan `.merge(...)` / `.route(...)` korumasız kalır. Örnek: `rest/mod.rs` `require_auth` / `require_upgrade_auth`.
- Global katmanlar (sayaç, rate limit, body limit, CORS) `rest::with_global_layers` ile **en son** uygulanır. Yeni bir router `main.rs`'de bu çağrıdan önce birleştirilir; sonra birleştirilen router bu katmanların dışında kalır (R-53, S-35).
- Durum değiştiren uçlar (yazma, silme, config, dosya, alarm/SMTP ayarı) varsayılan olarak auth içinde olmalı. Dışarıda kalacaksa gerekçesi kodda yorumla yazılır.
- Kimlik bilgisi ve token URL query'sine konmaz (R-14). Token karşılaştırması sabit zamanlı yapılır.
- GET yanıtlarında sır alanları maskelenir (örnek: `crates/sandstar-server/src/alerts.rs`).

## Dosya yolları
- İstekten gelen yol önce normalize edilir, ardından allow-list ile kontrol edilir, **en son** dosya veya dizin işlemi yapılır. R-15: `validate_path` dizini kontrolden önce oluşturuyor.
- Önek kontrolü string üzerinde değil, `Path::starts_with` ile ve kanonik yol üzerinde yapılır. `..` ve sembolik bağlantılar reddedilir.

## Uyumluluk
SkySpark, Sedona editörü ve `/live` Fantom lehçesi istemcileri mevcut yolları, parametre adlarını ve yanıt biçimlerini bekler. Bunlar değiştirilmez; gerekirse alias eklenir.
