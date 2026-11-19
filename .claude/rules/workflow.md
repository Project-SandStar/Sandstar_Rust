# Çalışma akışı (her zaman geçerli)

## Öncelik
Bu klasördeki proje kuralları (`.claude/rules/*.md`) `ecc-rust/` altındaki genel ECC kurallarıyla çelişirse **proje kuralları geçerlidir**. Bilinen farklar:
- ECC'nin "`expect` kabul" yaklaşımı yerine `panic-safety.md` uygulanır.
- %80 kapsama hedefi yok; kritik yollarda davranış testi aranır.
- ECC `hooks.md`'deki global hook önerileri kullanıcı onayı olmadan kurulmaz.

## "Bitti" demeden önce
Tek komut: `bash tools/gates.sh`. Kapı başına bir satır basar, yalnız kırmızı kapının çıktısını gösterir; kırmızı kapı varsa çıkış kodu 1 olur. Tek kapı çalıştırmak için `bash tools/gates.sh fmt clippy`. Son satır `docs` hatırlatmasıdır (aşağıda "Doküman disiplini"); kapı değildir, çıkış kodunu değiştirmez.
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace --no-fail-fast`. `--no-fail-fast` olmadan cargo ilk başarısız test binary'sinde durur ve geçen/başarısız sayımı eksik çıkar.
4. `cargo check -p sandstar-server --no-default-features --features linux-hal,svm`.
   - Gerçek ARM build gerekiyorsa:
     - Önce ortam betiği: `. C:\czb\arm-env.ps1` (Git Bash'te `source /c/czb/arm-env.sh`), sonra `cargo arm-build-svm`.
     - Ortam betiği olmadan C çapraz derleyicisi (`CC`/`AR`) bulunamadığı için build başarısız olur (R-21).
   - Toolchain `rust-toolchain.toml` ile 1.99.1'e sabit. Yükseltirken `ci.yml` ile birlikte değiştir.
5. Kapsama ölçümü (isteğe bağlı): `cargo llvm-cov -p <crate> --summary-only`.

Kırmızı bir kapı yeşil gösterilmez.
- Taban `master`'da ve `develop`'ta yeşil (R-37, S-5'te kapandı).
- Kararsız bir test bir koşuda düşer, tekrarda geçerse:
  - "geçti" denmez;
  - iki koşu da raporlanır;
  - test 08'de "Dokunulmayanlar"a yazılır.

## Doğrulama ve durum (karar 2026-11-18, EnvanterTakip'ten uyarlandı)
- **Durum basamakları:** 06'daki bir risk ya da bulgu şu sırayla ilerler:
  1. `🔄 kodda` (commit'lendi, testler geçti)
  2. `🔄 cihazda` (deploy edildi)
  3. `✅ sahada doğrulandı`

  ✅, sahada beklenen davranışın kanıtı görülmeden (günlük satırı, ölçüm, dosya durumu) verilmez. Kanıt, kimin doğruladığıyla birlikte yazılır: "Claude, salt okunur" ya da "kullanıcı". Sahaya gitmeyen işler (CI, araç, doküman) için ✅ = kapılar ve CI yeşil.
  - Gerekçe: R-41 kodda düzeltildi ve deploy edildi, sahada tek dosya silmedi.
- **Platforma bağlı davranış kaynaktan doğrulanır.**
  - Kapsam: dosya sistemi, sysfs, saat, izinler, sinyaller gibi hedef platforma bağlı kütüphane davranışları.
  - Doğrulama, kurulu sürümün kaynağından yapılır (`~/.cargo/registry/src/…/<crate>-<sürüm>/`), dokümana ya da hafızaya dayanılmaz.
  - Windows'ta yeşil test, cihazdaki Linux'u (kernel 4.9, armv7) kanıtlamaz.
  - Örnek: `tracing-appender` budaması dosya oluşturma zamanına dayanıyordu; cihazın dosya sisteminde bu zaman yoktu (R-41).

## Plan ve adım kaydı
- **Önce plan, onay, sonra kod.**
  - Kapsam: çok dosyaya yayılan, davranış değiştiren ya da cihaza giden S-n adımları.
  - Plan, uygulamadan önce 08'de adımın altına "Plan — ONAY BEKLİYOR" başlığıyla 5-10 satır yazılır:
    - koddaki mevcut durum (`dosya:satır`);
    - yapılacaklar;
    - testler;
    - inceleme ajanı;
    - riskler.
  - Onaydan sonra başlık "ONAYLANDI <tarih>" olur.
  - Plandan sapma "Plan revizyonu" satırıyla yazılır; sessiz sapma olmaz.
  - Tek dosyalık küçük düzeltmede plan gerekmez (global onay eşiği).
- **08 girdisi kısa tutulur.** Adım bitince "Sonuç" 5-10 satıra indirilir; ayrıntı commit gövdesinde ve PR'da yaşar.

## İnceleme ajanları
Ne zaman çağrılmayacağı global fren kuralındadır.

| Değişiklik | Ajan |
|---|---|
| `unsafe`, `sandstar-svm`, FFI, ownership/lifetime ağırlıklı değişiklik | `rust-reviewer` |
| Yeni ya da değişen rota, auth katmanı, tel protokolü ayrıştırıcısı (SOX/DASP, BACnet, roxWarp, `/live`), istekten gelen dosya yolu | `security-reviewer` |
| Sürücü, hata yolu, yeniden deneme, `unwrap_or`/`ok()` ile hata yutma ihtimali | `silent-failure-hunter` |
| Derleme ya da bağımlılık hatası | `rust-build-resolver` |

**Brifing:** ajana değişikliği (diff ya da dosyalar) ve sözleşmeyi (kabul ölçütü, ilgili `.claude/rules/` maddeleri) ver. Kendi gerekçeni ("şunu şu yüzden böyle yaptım") ve beklediğin sonucu **verme**; ajan kararı onaylamaya değil çürütmeye çalışsın.

## Hook'lar (`.claude/settings.json`, karar 2026-11-18)
- `.claude/hooks/protect-secrets.js` (PreToolUse; Read, Edit, Write, Grep, NotebookEdit):
  - **Sır dosyaları:** `.gitignore`'daki "Device/site credentials" bloğunda listelenen sır içeren `tools/` dosyalarını dosya araçlarıyla açmayı reddeder. Liste tek yerde, `.gitignore`'da tutulur; yeni sır dosyası oraya eklenir.
  - **`graphify-out/`:** bu dizine yazmayı reddeder.
  - **Bir güvenlik ağıdır, tam koruma değildir:** Bash ile okumayı engellemez. Sır içeren dosyaların içeriği Bash çıktısına da basılmaz; maskeleme yerine yalnız gereken bilgi (dosya adı, sayı, eşitlik) yazdırılır.

## Git ve commit (karar 2026-11-18)
Kaynak: kullanıcıyla konuşma; EnvanterTakip `DevelopingMds/03-GelistirmeKurallari.md` §8 incelendi, SandStar'a uyarlandı.
- **Kim, ne zaman:**
  - **Commit:** yalnız kullanıcı isteyince ya da onayıyla. Mantıksal bir değişiklik bitip testler geçince Claude "commit atayım mı?" diye sorar, kendiliğinden atmaz.
  - **`develop`'a alma:** S adımı bitince, kapılar ve PR'ın CI'ı yeşilse sorulmaz (karar 2026-11-19). Sonuç kullanıcıya bildirilir.
  - **`master`'a alma:** yalnız kilit bir geliştirme bitince (ör. sahaya gidecek sürüm, bir fazın kapanışı). Claude önerir, kullanıcı onaylar.
  - **Dalı push etmek ve PR açmak:** sorulmaz (karar 2026-11-18). CI'ın koşması için yapılır, sonuç kullanıcıya bildirilir.
  - **Hiçbir koşulda serbest değil:** `master`'a ya da `develop`'a PR'sız commit, `--force` push.
- **Dal modeli (karar 2026-11-19, kullanıcı):**
  - `master`: kararlı; sahaya giden sürümler buradan derlenir.
  - `develop`: entegrasyon dalı. Her S adımı `develop`'tan açılan kendi dalında yapılır: `s<n>-<konu>` (ör. `s12-develop-flow`).
  - Akış: `develop` → `s<n>-<konu>` → PR (taban `develop`) → `develop`. Kilit geliştirmeden sonra `develop` → PR (taban `master`) → `master`.
  - Birleşen adım dalı yerelde ve uzakta silinir; böylece yalnız `master` ve `develop` kalıcıdır.
  - Gerekçe: uzak depo bir organizasyona ait (`Project-SandStar/Sandstar_Rust`) ve kod sahadaki cihaza gidiyor; `master` yalnız doğrulanmış toplu değişiklik alır.
  - Dal içinde kırmızı kapıyla ara commit kabul edilir; mesaj gövdesinde hangi kapının neden kırmızı kaldığı yazılır.
  - Alma şartı (`develop` ve `master`): aşağıdaki kontrol listesi ve CI `Test` işi yeşil.
  - Birleştirme yalnız GitHub'dan (karar 2026-11-19):
    - adım dalı → `develop`: `gh pr merge <n> --rebase --delete-branch`. Atomik commit'ler korunur, hash'ler değişir; 08'e birleşme sonrası hash'ler yazılır.
    - `develop` → `master`: `gh pr merge <n> --merge`. Her kilit geliştirme `master`'da tek bir birleşme commit'i olur.
    - Birleştirmeden sonra yerelde `git pull --ff-only`; adım dalı yerelde `git branch -D` ile silinir (rebase sonrası hash'ler farklı olduğu için `-d` reddeder).
- **GitHub ruleset'leri (karar 2026-11-19, kullanıcı):** `master` ve `develop` için ayrı ruleset, istisnasız (adminler dahil):
  - dal silinemez, force push yasak;
  - değişiklik yalnız PR ile; onay sayısı 0 (tek geliştirici; kendi PR'ını onaylamak mümkün değil);
  - CI `Test` işi geçmeden birleşmez;
  - izinli yöntem: `develop` için yalnız rebase, `master` için yalnız merge commit.
  - Ruleset'i değiştirmek ya da kapatmak kullanıcı kararıdır; organizasyondaki diğer admini de bağlar.
  - `Build ARM` işi PR'da koşmadığı için zorunlu değil; ARM kırılması birleşmeden sonra push CI'ında görülür.
  - Sahaya deploy `master`'dan yapılır. `develop`'tan deploy yalnız kullanıcı açıkça isterse (ör. sahada deneme); 08'e hangi daldan ve hangi hash'ten deploy edildiği yazılır.
- **Mesaj biçimi:** `tür(kapsam): özet`, İngilizce, emir kipinde, ~72 karakter.
  - Türler: `feat`, `fix`, `refactor`, `perf`, `test`, `docs`, `style` (yalnız biçim), `ci`, `build`, `chore`.
  - Kapsam: crate ya da modül adı (`rest`, `live`, `sox`, `bacnet`, `mqtt`, `roxwarp`, `engine`, `hal`, `svm`, `cli`, `deb`, `tools`, `ci`).
  - İlgili kimlikler özetin sonunda parantez içinde: `fix(rest): reject NaN in write path (R-9, S-6)`. Faz adı gerekiyorsa gövdeye yazılır.
  - Eski biçim (`Phase 14.0G-3: …`, `Fix …`) geçmişte kalır.
- **Gövde:** ne değişti ve neden; davranış etkisi ("No behavior change" ya da açıklaması); test sonucu (geçen/başarısız sayısı); kırmızı kalan kapı. Kurallar ayrıca şunları ister:
  - rota → auth katmanının içinde mi dışında mı (`routes-auth.md`);
  - tel protokolü değişikliği → gerekçe ve etkilenen istemciler (`wire-protocols.md`).
- **Atomik commit:** tek mantıksal değişiklik, tek commit. Yalnız biçim değişikliği (`cargo fmt`) ayrı bir `style:` commit'idir ve mantıksal değişiklikle karıştırılmaz; biçim farkı varsa önce o commit'lenir.
- **Commit öncesi kontrol listesi:** aşağıdaki "Bitti demeden önce" 1-4; ayrıca kurallara göre gerekenler:
  - çıkış yoluna dokunulduysa `is_finite` testi (`numeric-output.md`);
  - yeni ayrıştırıcı ya da handler için boş, kısa, bozuk ve UTF-8 girdi testleri (`panic-safety.md`, `wire-protocols.md`).
- **Commit'e girmeyenler:** `tools/connections/` ve parola içeren `tools/` betikleri (`.gitignore`), `.playwright-mcp/`, ekran görüntüleri. `docs/DevelopingMds/` `.git/info/exclude` ile bilerek repo dışında.
- **Sondaki satırlar:** sistemin eklediği `Co-Authored-By: Claude …` ve `Claude-Session: …` satırları kalır.
- **Adım kaydı:** S-n bitince `08-ILERLEME.md`'deki adım sonucuna dal adı, commit hash(ler)i ve bir "Dokunulmayanlar" satırı yazılır: uygulama sırasında fark edilip kapsam dışı bırakılan konular. Bunlar sessizce düzeltilmez, gerekiyorsa 06'ya risk olarak açılır.

## Sürüm ve kayıt
- Sürüm değişikliğinde `Cargo.toml` `[workspace.package] version`, `CHANGELOG.md` ve git tag birlikte güncellenir.
- Her adım (S-n) bitince `docs/DevelopingMds/` güncellenir: en az `08-ILERLEME.md`, gerekiyorsa 06, 07 ve 10.

## Doküman disiplini (karar 2026-11-19, EnvanterTakip `CLAUDE.md` §9'dan uyarlandı)
Gerekçe: git'teki `docs/Progress` ve `docs/research` altı ay koddan koptu (R-32, S-10'da toplu düzeltildi). Kural, kod değişikliğiyle aynı dalda ilgili dokümanı güncellemektir.

| Kodda değişen | Güncellenecek doküman |
|---|---|
| Rota ya da auth katmanı | `docs/Progress/OVERVIEW.md` §3 rota/auth tablosu; sürücü rotasıysa `DRIVER_REST_API.md` |
| Sabit, limit, CLI varsayılanı | `docs/Progress/HARDCODED_LIMITS.md` |
| `.deb` içeriği, unit dosyası, `initialize.sh`, deploy betiği, doğrulamada okunan log satırı | `docs/Progress/DEPLOYMENT_CHECKLIST.md` |
| BACnet ya da MQTT sürücü davranışı | `BACNET_SETUP.md` / `MQTT_SETUP.md` |
| Bir araştırma alanındaki iş (VM, roxWarp, HAL, Zinc/filter, dinamik slot, sürücü çatısı, build/CI) | İlgili `docs/research/NN_*.md` Status bloğu: uygulanan / farklı yapılan / açık listesi hâlâ doğru mu |
| Sürüm | `Cargo.toml`, `CHANGELOG.md`, git tag ve OVERVIEW başlığı birlikte |
| Risk açıldı, ilerledi, kapandı | 06 (durum basamağı ve kanıtla) |
| Her S-n adımı | 08, gerekiyorsa 10 |

- **Hatırlatma:** `tools/gates.sh` sonundaki `docs` satırı, dalın `develop` ile (yoksa `master` ile) ortak atasına göre değişen yolları `tools/docs-map.tsv` ile eşler. Kod yolu değişip doküman değişmediyse dokümanı listeler. Eşlemenin tek kaynağı `docs-map.tsv`'dir; yukarıdaki tablo onun okunur özetidir. İkisi birlikte değişir.
- **Hatırlatma kapı değildir.** Doküman gerekmiyorsa (ör. yalnız yorum ya da test değişti) atlanır; commit gövdesine bir satır yazılır: "docs: <doküman> gerekmedi, çünkü …".
- DevelopingMds git dışında olduğu için hatırlatma onu göremez; 08/06/10 için yukarıdaki "Sürüm ve kayıt" geçerlidir.
- Dokümanda sonuç doğrulanmadan "tamamlandı" yazılmaz; sahaya giden işlerde durum basamakları (yukarıda) uygulanır.

## Cihaz ve saha
- BeagleBone'a SSH, deploy, servis yeniden başlatma, cutover veya rollback yalnız açık kullanıcı onayıyla yapılır. Deploy betikleri host key doğrulamasını kapatıyor (R-22).
- **Onay oturum başınadır (karar 2026-11-18):** kullanıcı bir oturumda cihaz erişimine onay verince, o oturumda şunlar ayrıca sorulmaz:
  - salt okunur SSH ve `sudo` ile okuma komutları;
  - onaylanan işin kapsamındaki yazmalar (ör. onaylı deploy'un yedeği, kurulumu, servis yeniden başlatması).

  Şunlar yine sorulur:
  - kapsam dışı yeni bir yazma türü (cihaz ayarı, GPIO/sysfs yazma, paket kaldırma, rollback, cutover);
  - geri dönüşü zor işlem (silme).

  Deploy öncesi yedek kuralı değişmez.
- BACnet/MQTT ile gerçek ekipmana yazma testi yapılmaz. Simülatörleri kullan: `tools/bacnet_sim.py`, yerel mosquitto, `cargo sim-run`.
- Host, IP, kullanıcı adı ve parola değerleri dokümana veya koda yazılmaz.

## Editör (harici repo)
Vue editörü `C:\Coding\swe\sedonaWebEditor` yolunda, aktif dal `trkrDevelop`. Backend'de `/live`, `/live/sedonaState` ve `/api/*` sözleşmesini değiştiren işler editör tarafıyla birlikte düşünülür.
