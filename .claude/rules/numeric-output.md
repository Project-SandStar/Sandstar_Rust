---
paths:
  - "crates/sandstar-engine/**/*.rs"
  - "crates/sandstar-hal/**/*.rs"
  - "crates/sandstar-hal-linux/**/*.rs"
  - "crates/sandstar-server/src/drivers/**/*.rs"
  - "crates/sandstar-server/src/control.rs"
  - "crates/sandstar-server/src/sox/**/*.rs"
---
# Sayısal değerler ve fiziksel çıkış

Bu kod gerçek ekipmanı (vana, damper, fan, PWM) sürer. Yanlış bir sayı fiziksel bir hata demektir.

## Kurallar
- **Çıkışa giden her değer `is_finite()` kontrolünden geçer.** NaN veya ±inf ya **reddedilir** ve hata döner ya da yazma atlanır. Asla min/max'a sıkıştırılmaz. R-9: `ValueConv::revert` NaN'ı max'a çekiyor, bu kalıp tekrarlanmaz.
- Okuma hatası, kopuk sensör ya da tanımsız girdi **0 sayılmaz** (R-35). "Değer yok" durumu `Option` veya status ile taşınır.
- Dönüşüm, ölçekleme ve sinyal filtrelerinde, tablo/lookup sınırları dışında ve hata sayacı eşiğinde ne olacağı açıkça tanımlanır (R-30).
- Tamsayıya dönüşümde (`as u16` vb.) önce aralık kontrolü yapılır. `as` sessizce keser ya da doyurur.

## Priority array (17 seviye)
- Yazmalar `channel_write_level(level, who, duration)` üzerinden gider. Seviye bilinçli seçilir; yeni bir kaynak eklerken mevcut atamalara uy:
  - control/PID: 8 (`crates/sandstar-server/src/main.rs:685-698`)
  - SOX `w`: 8
  - logic → channel: 17
  
  Ayrıntı: `docs/DevelopingMds/02-AKISLAR.md` §2, §3, §5.
- `duration > 0` süreli yazmadır ve zamanlayıcıyla düşer; `0` süresizdir.

## Test
Yeni bir dönüşüm ya da çıkış yolu eklerken NaN, ±inf, sınır dışı değer ve okuma hatası için test yaz.
