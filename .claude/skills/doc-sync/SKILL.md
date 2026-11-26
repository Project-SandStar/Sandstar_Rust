---
name: doc-sync
description: SandStar'da kod değişikliğinden, sürüm artışından ya da deploy'dan sonra hangi dokümanların eskidiğini bulur ve günceller (README, CHANGELOG, docs/Progress, docs/research durum blokları, docs/DevelopingMds 06/08/10). Bir S adımı bitince, "commit atayım mı?" diye sormadan önce, sürüm numarası değişince, cihaza deploy yapılınca ve kullanıcı "dokümanları güncelle", "doküman eskimiş mi", "docs güncel mi", "doc-sync" dediğinde kullan. `tools/gates.sh` içindeki `docs` hatırlatması yalnız eşlemede tanımlı kalıpları yakalar; düz yazı açıklamalarını, sayıları ve "cihazda kurulu sürüm" gibi durum satırlarını yakalamaz. O boşluğu bu skill kapatır, bu yüzden hatırlatma "no reminders" dese bile çalıştır.
---

# doc-sync: değişiklikten sonra dokümanları eşitleme

## Neden var

Bu projede dokümanlar iki kez koddan koptu. İlki R-32'dir (altı ay; S-10'da toplu düzeltildi). İkincisi kural ve hatırlatma varken oldu:

- `OVERVIEW.md` "SIGTERM zarif kapanış yapmaz" diyordu; kod aylardır yapıyordu (S-63'te fark edildi).
- 2.10.0 cihaza kuruldu, `OVERVIEW.md` "Deployed: 2.9.0" demeye devam etti.
- `DEPLOYMENT_CHECKLIST.md` doğrulama adımı "2.9.x" diyordu.
- README'deki test sayısı her adımda eskidi.

Ortak neden: `tools/gates.sh docs` hatırlatması bir **kod yolu → doküman** eşlemesidir (`tools/docs-map.tsv`). Eşlemede olmayan dosyayı, bir dokümanın düz yazıyla anlattığı davranışı ve sayıları göremez. Bu skill üç geçişle o boşluğu kapatır: mekanik hatırlatma, iddia taraması, durum satırları.

## Ne zaman, hangi kapsamla

Önce hangi olayın bittiğini belirle; kapsam ona göre değişir.

| Olay | Karşılaştırma tabanı | Ek olarak |
|---|---|---|
| S adımı (dalda, commit öncesi) | `git merge-base HEAD develop` | — |
| Sürüm artışı | en yüksek sürüm etiketi (`git tag --sort=-v:refname` çıktısının ilk satırı; `git describe` `develop`'ta en yeni etiketi görmez) | Sürüm satırları (aşağıda "Durum satırları") |
| Deploy | kurulu eski sürümün hash'i … kurulan hash | "Deployed" satırları, 06 durum basamakları |
| "Dokümanlar güncel mi?" (genel denetim) | son etiket … `HEAD` | Üç geçişin hepsi, tüm `docs/Progress` için |

Yalnız test ya da yalnız yorum değişen adımda dokümana gerek yoktur; bunu bir cümleyle söyle ve dur.

## Geçiş 1: mekanik hatırlatma

```bash
bash tools/gates.sh docs
```

Listelenen her doküman için ilgili bölümü oku ve değişiklikle karşılaştır. Gerekmiyorsa neden gerekmediğini not et; bu not commit gövdesine "docs: <doküman> gerekmedi, çünkü …" olarak girer (`workflow.md`, "Doküman disiplini").

## Geçiş 2: iddia taraması

Hatırlatmanın göremediği kısım budur ve asıl iş buradadır.

1. **Değişen davranışı kendi cümlenle yaz.** Diff'ten ve commit mesajından 3-8 madde çıkar: "X artık Y yapıyor", "Z artık hata döndürüyor". Davranış değişmediyse (taşıma, yeniden adlandırma) bile dosya yolu ve satır başvuruları eskimiş olabilir; onları da madde yap.
2. **Her madde için anahtar sözcük seç.** Kullanıcının ve operatörün göreceği adlar işe yarar: CLI komutu (`reload`), rota (`/api/reload`), sinyal (`SIGHUP`), dosya adı (`control.toml`), günlük metni (`config reload complete`), bayrak (`--no-control`), taşınan dosyanın eski yolu (`main.rs`). İç işlev adları dokümanlarda nadiren geçer.
3. **Dokümanlarda ara:**
   ```bash
   grep -rn -i "<anahtar>" README.md CHANGELOG.md docs/Progress docs/research .claude/rules CLAUDE.md
   ```
4. **Her isabeti oku ve karar ver:** cümle hâlâ doğru mu? Üç sonuç var: doğru (dokunma), yanlış (düzelt), artık eksik (yeni davranışı ekle). İsabet sayısı çoksa hepsini okumak için bir `Explore` ajanı kullanılabilir; karar yine bu oturumda verilir.
5. **Hiç isabet yoksa** davranışın yazılması gereken bir yer var mı diye düşün: operatörün göreceği bir değişiklik `CHANGELOG.md` ve çoğunlukla `OVERVIEW.md` ister; doğrulamada okunan bir günlük satırı `DEPLOYMENT_CHECKLIST.md` §7 ister.

Değişiklik türüne göre nereye bakılacağı ve her dokümanın neyi iddia ettiği `references/dokuman-haritasi.md` dosyasında. İlk kez kullanırken ya da emin değilken oku.

## Geçiş 3: durum satırları

Bunlar hiçbir kod yoluna bağlı olmayan, kendiliğinden eskiyen satırlardır. Betik güncel değerleri ve dokümanlarda geçtikleri yerleri yan yana basar:

```bash
bash .claude/skills/doc-sync/scripts/doc-facts.sh
```

Çıktıdaki her satır için değer hâlâ doğru mu bak:

- **Sürüm:** `Cargo.toml` ile README başlığı, `OVERVIEW.md` başlığı, `CHANGELOG.md` en üst bölüm aynı sürümü söylemeli. Sürüm artışında git etiketi de birlikte gider (etiket ayrı kullanıcı onayı ister).
- **Cihazda kurulu sürüm:** yalnız deploy doğrulandıktan sonra değişir; değer `/api/about` çıktısından alınır, tahminle yazılmaz.
- **Test sayısı:** son `bash tools/gates.sh` koşusunun `passed=` değeri. Kapıyı bu adımda koşmadıysan sayıyı uydurma; koş ya da eski sayıyı bırakıp raporda söyle.
- **Doğrulama adımındaki sürüm kalıbı** (`DEPLOYMENT_CHECKLIST.md` §7).

## DevelopingMds (git dışında)

`docs/DevelopingMds/` hatırlatmanın görmediği ikinci alandır; her S adımında elle güncellenir. Biçimi ezberden yazma, dosyadaki son girdiyi örnek al.

- **08-ILERLEME.md:** adım girdisi (plan durumu, plan revizyonu, sonuç 5-10 satır, dokunulmayanlar, birleşme hash'leri).
- **06-RISKLER.md:** dokunulan riskin durum basamağı: `🔄 kodda` → `🔄 cihazda` → `✅ sahada doğrulandı`. ✅ için sahadaki kanıt ve kimin doğruladığı yazılır.
- **10-KALDIGIMIZ-YER.md:** adım satırı ve "cihazda olmayan adımlar".
- **Planlar/…:** plan dosyasının "Durum" satırı ve uygulama kaydı.
- **07, 09:** yalnız açık soru kapandıysa ya da araç değerlendirmesi yapıldıysa.

06'da bir riskin "Kanıt" satırı `dosya:satır` taşır; kod taşındıysa bu başvurular da eskir.

## Eşlemeyi büyüt

Geçiş 2 ya da 3'te, hatırlatmanın yakalaması gereken bir eksik bulduysan (ör. bir dosyadaki günlük satırı bir dokümanın doğrulama adımında okunuyor), `tools/docs-map.tsv`'ye bir satır öner. Eşleme `workflow.md`'deki tabloyla birlikte değişir; ikisi de git'te olduğu için bu bir kod değişikliğidir ve adımın dalında yapılır. Böylece aynı eksik bir sonraki sefer kendiliğinden görünür.

## Yazarken

- Git'teki dokümanlar İngilizce, `docs/DevelopingMds/` Türkçe.
- Host, IP, kullanıcı adı ve parola hiçbir dokümana yazılmaz. `tools/` altındaki sır dosyaları açılmaz.
- Doğrulanmamış şey "tamamlandı" ya da "✅" diye yazılmaz. Sahada görülmeyen davranış "kodda" kalır.
- Yeni doküman açmadan önce mevcut dokümanda yerini ara.
- Yalnız bu değişikliğin eskittiği satırlara dokun. Başka bir eski satır fark edersen düzeltme; raporda "Fark edilenler" altında yaz, kullanıcı karar versin. (Küçük ve aynı paragraftaysa düzeltip raporda belirt.)

## Rapor

İş bitince kullanıcıya şu yapıda kısa bir özet ver:

```
**Güncellenenler:** <doküman>: <ne değişti> (her biri tek satır)
**Bakılıp dokunulmayanlar:** <doküman>: <neden hâlâ doğru>
**Doğrulanamayanlar:** <hangi satır, doğrulamak için ne gerekiyor>
**Fark edilenler (kapsam dışı):** <başka eski satırlar>
**Eşleme önerisi:** <docs-map.tsv satırı ya da "yok">
```

"Doğrulanamayanlar" boş bırakılmaz diye bir kural yok; ama bir satırı okumadan "hâlâ doğru" deme.
