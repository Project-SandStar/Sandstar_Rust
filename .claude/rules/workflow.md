# Çalışma akışı (her zaman geçerli)

## Öncelik
Bu klasördeki proje kuralları (`.claude/rules/*.md`) `ecc-rust/` altındaki genel ECC kurallarıyla çelişirse **proje kuralları geçerlidir**. Bilinen farklar:
- ECC'nin "`expect` kabul" yaklaşımı yerine `panic-safety.md` uygulanır.
- %80 kapsama hedefi yok; kritik yollarda davranış testi aranır.
- ECC `hooks.md`'deki global hook önerileri kullanıcı onayı olmadan kurulmaz.

## "Bitti" demeden önce
1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace`
4. `crates/sandstar-hal-linux` veya `linux-hal`/`svm` feature'larına dokunulduysa: `cargo check -p sandstar-server --no-default-features --features linux-hal,svm`. Gerekiyorsa gerçek ARM build: önce `. C:\czb\arm-env.ps1` (Git Bash: `source /c/czb/arm-env.sh`), sonra `cargo arm-build-svm`. Ortam betiği olmadan C çapraz derleyicisi (`CC`/`AR`) bulunamadığı için başarısız olur (R-21). Toolchain `rust-toolchain.toml` ile 1.99.1'e sabit; yükseltirken `ci.yml` ile birlikte değiştir.
5. Kapsama ölçümü (isteğe bağlı): `cargo llvm-cov -p <crate> --summary-only`.

Kırmızı bir kapı yeşil gösterilmez. Mevcut taban: fmt ve clippy HEAD'de zaten kırmızı (R-37). Senin değişikliğin **yeni** hata eklememeli; tabanı düzeltmek ayrı bir adımdır.

## Git ve commit (karar 2026-11-18)
Kaynak: kullanıcıyla konuşma; EnvanterTakip `DevelopingMds/03-GelistirmeKurallari.md` §8 incelendi, SandStar'a uyarlandı.
- **Kim, ne zaman:** commit, `master`'a alma ve push yalnız kullanıcı isteyince ya da onayıyla. Mantıksal bir değişiklik bitip testler geçince Claude "commit atayım mı?" diye sorar, kendiliğinden atmaz.
- **Dal:** her S adımı kendi dalında: `s<n>-<konu>` (ör. `s5-ci-gates`). Doğrudan `master`'a commit yok.
  - Gerekçe: uzak depo bir organizasyona ait (`Project-SandStar/Sandstar_Rust`) ve kod sahadaki cihaza gidiyor.
  - Dal içinde kırmızı kapıyla ara commit kabul edilir; mesaj gövdesinde hangi kapının neden kırmızı kaldığı yazılır.
  - `master`'a alma şartı: aşağıdaki kontrol listesi yeşil. Fast-forward tercih edilir; olmazsa `--no-ff` merge.
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

## Cihaz ve saha
- BeagleBone'a SSH, deploy, servis yeniden başlatma, cutover veya rollback yalnız açık kullanıcı onayıyla yapılır. Deploy betikleri host key doğrulamasını kapatıyor (R-22).
- BACnet/MQTT ile gerçek ekipmana yazma testi yapılmaz. Simülatörleri kullan: `tools/bacnet_sim.py`, yerel mosquitto, `cargo sim-run`.
- Host, IP, kullanıcı adı ve parola değerleri dokümana veya koda yazılmaz.

## Editör (harici repo)
Vue editörü `C:\Coding\swe\sedonaWebEditor` yolunda, aktif dal `trkrDevelop`. Backend'de `/live`, `/live/sedonaState` ve `/api/*` sözleşmesini değiştiren işler editör tarafıyla birlikte düşünülür.
