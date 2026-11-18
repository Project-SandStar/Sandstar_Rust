---
paths:
  - "crates/**/*.rs"
---
# Panic güvenliği (release `panic = "abort"`)

Release profilinde her panic süreci düşürür (`.cargo/config.toml` `[profile.release]`). Ağdan, REST'ten, config'den veya dosyadan gelen veri, panic'e giden bir yol açmamalı. Kaynak: `docs/DevelopingMds/06-RISKLER.md` R-4…R-10.

## Dış girdi yolunda yasak
- `unwrap()` / `expect()`: ikisi de aynı şekilde süreci düşürür. `expect` güvenli bir alternatif **değildir**. Hata döndür (`?`, `ok_or`, `map_err`).
- `buf[i]`, `buf[a..b]`: önce uzunluğu kontrol et ya da `.get(..)` kullan. Örnek: R-5 (BVLL < 4 bayt), R-8 (Modbus).
- `&s[a..b]` ile `str` dilimi: UTF-8 karakter sınırında panic verir (R-6). `char_indices`, `is_char_boundary` veya `.get(a..b)` kullan.
- Karakter indeksiyle bayt indeksini karıştırmak (R-7): `chars().position` sonucu bayt dilimlemede kullanılmaz.
- `f64::clamp(lo, hi)`: `lo > hi` ya da NaN girdiyle panic verir (R-10). Sınırlar config'ten geliyorsa önce doğrula.
- `Duration::from_secs_f64(x)`: negatif, NaN veya taşan değerle panic verir. `try_from_secs_f64` kullan ya da önce doğrula.
- Bölme ve aritmetik: payda sıfır olabilecek tamsayı bölmelerinde `checked_div` veya ön kontrol. Ağdan gelen uzunluk ve sayaçlarda `checked_*` veya `saturating_*`.

## Kabul edilen kullanımlar
- Testler (`#[cfg(test)]`, `tests/`): `unwrap` serbest.
- Program başlangıcında, hiçbir istek işlenmeden önce, düzeltilemez yapılandırma hatası: `expect("<neden>")` kabul edilir.
- Değişmezliği koddan kanıtlanabilen durumlar: yanına `// INVARIANT: …` yorumu yaz.

## Kontrol
Yeni bir ayrıştırıcı ya da handler eklerken boş, kısa, bozuk ve UTF-8 çok baytlı girdiler için test yaz.
