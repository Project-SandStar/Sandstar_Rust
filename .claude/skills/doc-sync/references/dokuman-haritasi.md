# Doküman haritası

Hangi doküman neyi iddia eder ve hangi değişiklik onu eskitir. `tools/docs-map.tsv` mekanik eşlemedir; bu dosya onun göremediklerini de içerir.

## Git'teki dokümanlar

| Doküman | Ne iddia eder | Ne eskitir |
|---|---|---|
| `README.md` | Başlıkta sürüm, cihaz durumu, test sayısı; crate listesi; rota tablosu; betik tablosu; "Testing" bölümünde test sayısı; "Development" bölümü | Sürüm artışı, deploy, her test ekleyen adım, rota davranışı, `tools/` altına eklenen ya da çıkan betik |
| `CHANGELOG.md` | Sürüm başına değişiklikler; "Behavior changes (read before upgrading)" | Operatörün ya da istemcinin göreceği her değişiklik. `develop`'taki işler `## [Unreleased]` altında birikir; sürüm artışında başlık sürüm numarası ve tarih olur |
| `docs/Progress/OVERVIEW.md` | Başlıkta güncel ve kurulu sürüm; §3 rota/auth tablosu ve çalışma zamanı davranışları (zaman aşımları, sinyaller, yeniden yükleme); §4 deploy durumu; sürüm geçmişi tablosu | Rota ya da auth katmanı, sinyal ve kapanış davranışı, yeniden yükleme, zaman aşımı, sürüm artışı, deploy |
| `docs/Progress/DEPLOYMENT_CHECKLIST.md` | Paket içeriği, kurulum adımları, §7 doğrulamada okunan günlük satırları ve beklenen değerler, geri dönüş | `.deb` içeriği, unit dosyası, `initialize.sh`, deploy betiği; doğrulamada okunan herhangi bir günlük satırının metni ya da beklenen değeri (hangi dosyada olursa olsun); sürüm kalıbı |
| `docs/Progress/HARDCODED_LIMITS.md` | Sabitler, limitler, CLI varsayılanları ve bulundukları `dosya:satır` | Yeni ya da değişen sabit; **kod taşıma** (satır ve dosya başvuruları kayar) |
| `docs/Progress/DRIVER_REST_API.md` | Sürücü rotaları ve gövdeleri | `drivers/` altındaki rota değişikliği |
| `docs/Progress/BACNET_SETUP.md`, `MQTT_SETUP.md` | Sürücü yapılandırması, varsayılanlar, günlük metinleri | İlgili sürücünün davranışı |
| `docs/research/NN_*.md` | En üstteki Status bloğu: uygulanan, farklı yapılan, açık kalan | O araştırma alanındaki iş (VM, roxWarp, HAL, Zinc/filter, dinamik slot, sürücü çatısı, build/CI) |
| `.claude/rules/*.md`, `CLAUDE.md` | Kurallar ve bazen `dosya` başvuruları, örnek kod yerleri | Kod taşıma; bir kuralın dayandığı kararın değişmesi (kullanıcıyla konuşulur) |
| `SECURITY.md` | Desteklenen sürümler | Sürüm artışı (ana ya da küçük sürüm) |

## Değişiklik türüne göre bakılacak yerler

- **Yeni ya da değişen rota, auth:** OVERVIEW §3 tablo, README rota tablosu, sürücü rotasıysa DRIVER_REST_API.
- **Operatörün gördüğü davranış** (CLI çıktısı, cevap metni, hata koşulu): CHANGELOG, OVERVIEW §3, README'de ilgili satır.
- **Günlük satırı metni ya da beklenen değer:** DEPLOYMENT_CHECKLIST §7; sürücüyse SETUP dokümanı.
- **Sinyal, kapanış, yeniden yükleme, olay döngüsü:** OVERVIEW §3 sonundaki maddeler, DEPLOYMENT_CHECKLIST §7 (kapanış satırları), `.claude/rules/engine-async.md`.
- **Kod taşıma ya da dosya bölme** (davranış aynı): HARDCODED_LIMITS satır başvuruları, `.claude/rules/` içindeki dosya göstergeleri, 06'daki "Kanıt" satırları, CHANGELOG "Internal".
- **Sabit, limit, varsayılan:** HARDCODED_LIMITS.
- **Paket, unit, betik:** DEPLOYMENT_CHECKLIST, README betik tablosu.
- **Sürüm artışı:** `Cargo.toml`, `Cargo.lock`, CHANGELOG başlığı, README başlığı ve Testing satırı, OVERVIEW başlığı ve sürüm geçmişi tablosu, SECURITY, DEPLOYMENT_CHECKLIST sürüm kalıbı; etiket ayrı onayla.
- **Deploy:** OVERVIEW "Deployed" ve §4, README başlığındaki cihaz sürümü, 06 durum basamakları (`🔄 cihazda`, kanıt varsa `✅`), 08 (hangi dal, hangi hash, yedek adı), 10 ("cihazda olmayan adımlar" listesi boşalır).

## `docs/DevelopingMds/` (git dışında, Türkçe)

| Dosya | Ne zaman |
|---|---|
| `08-ILERLEME.md` | Her adım |
| `06-RISKLER.md` | Risk açıldı, ilerledi, kapandı; kanıt başvurusu taşındı |
| `10-KALDIGIMIZ-YER.md` | Her adım (tek satır) ve deploy |
| `Planlar/<tarih>-S-<n>-….md` | Planlı adımlarda durum ve uygulama kaydı |
| `07-ACIK-SORULAR.md` | Bir soru cevaplandı ya da yenisi çıktı |
| `09-ARAC-ONERILERI.md` | Araç değerlendirmesi yapıldı |
| `01`-`05`, `11` | Mimari, akış, veri modeli ya da protokol değişti (nadir) |
