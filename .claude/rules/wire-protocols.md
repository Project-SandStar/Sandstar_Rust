---
paths:
  - "crates/sandstar-server/src/sox/**/*.rs"
  - "crates/sandstar-server/src/drivers/bacnet/**/*.rs"
  - "crates/sandstar-server/src/drivers/modbus.rs"
  - "crates/sandstar-server/src/drivers/mqtt.rs"
  - "crates/sandstar-server/src/roxwarp/**/*.rs"
  - "crates/sandstar-server/src/rest/live.rs"
  - "crates/sandstar-server/src/rest/rows.rs"
  - "crates/sandstar-server/src/rest/sedona_state.rs"
---
# Tel protokolleri (SOX/DASP, BACnet/IP, Modbus, MQTT, roxWarp, /live, RoWS)

Bu modüller ağdan gelen ve doğrulanmamış baytları ayrıştırır. Karşı tarafta değiştiremeyeceğimiz istemciler var: C Sedona, Sedona editörü, SkySpark ve sahadaki BACnet cihazları. Protokol haritası: `docs/DevelopingMds/11-PROTOKOLLER.md`.

## Ayrıştırma
- Her okuma öncesinde kalan uzunluk kontrol edilir. Başlıktaki uzunluk alanına güvenilmez; tampon boyutuyla karşılaştırılır (R-5, R-8).
- Ayrıştırıcı panic vermez, `Result` döner. Bozuk paket loglanır (oranı sınırlı) ve atılır; bağlantı veya süreç düşmez.
- Komut ve opcode karşılaştırmaları protokol sabitleriyle yapılır, harf literalleriyle değil. R-16: `b'k'`, Invoke `b'i'` yerine yazılmış.
- Bayt sırası her alanda açıkça belirtilir (`from_be_bytes` / `from_le_bytes`).
- Kaynak doğrulaması: yanıt beklenen adres ve porttan mı geliyor, bakılır (R-34).

## Uyumluluk
- Wire formatı, slot sırası, kod çözme (ör. Fantom `encodeString`) ve hata kodları **sessizce değiştirilmez**. Değişiklik gerekiyorsa gerekçesi ve etkilenen istemciler commit mesajında yazılır.
- Referans davranış C Sedona / SkySpark'tır. Emin değilsen tahmin etme, sor.

## Test
Her ayrıştırıcı için şu girdilerle test bulunur:
- boş, 1 bayt ve başlıktan kısa tampon
- başlıktaki uzunluk tampondan büyük
- bilinmeyen opcode
- çok baytlı UTF-8 içeren metin
