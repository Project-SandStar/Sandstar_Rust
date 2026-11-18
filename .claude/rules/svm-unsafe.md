---
paths:
  - "crates/sandstar-svm/**/*.rs"
---
# sandstar-svm (Sedona VM) ve unsafe

**Durum:** SVM üretimde kapalı ve gerçek scode'u çalıştıramıyor. `CallNative`'e boş `Vec` geçiyor, runner header okumuyor (R-11). Düzeltilecek mi, "deneysel" diye mi işaretlenecek, karar Q-6'ya bağlı. Karar verilmeden SVM'e büyük yatırım yapılmaz; önce sor.

## unsafe
- Her `unsafe` bloğun hemen üstünde `// SAFETY:` yorumu olur: hangi değişmez neden geçerli.
- Raw pointer aritmetiği VM bellek sınırlarına göre kontrol edilir. 32-bit ARM hedefinde `usize` 32 bittir; Sedona adreslerini pointer'a çevirirken taşma ve sınır kontrolü yapılır (R-12, `crates/sandstar-svm/src/native_sys.rs:155-165,373-375`).
- Yeni `unsafe` yerine önce güvenli bir alternatif aranır (slice + indeks kontrolü). `unsafe` performans gerekçesiyle eklenecekse ölçümle desteklenir.
- FFI ve native tablolarda imzalar C Sedona ile eşleşir; bayt sırası ve hizalama açıkça belirtilir.

## Test
Gerçek scode testleri dosya yoksa atlanıyor (`crates/sandstar-svm/tests/integration_tests.rs:23-30`). Yeşil bir test sonucu SVM'in çalıştığı anlamına gelmez (R-33).
