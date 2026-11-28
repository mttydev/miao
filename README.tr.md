<p align="center">
  <strong>miao</strong>
</p>
<p align="center">Kendi modelleriniz. Kendi iş akışınız. Daha az boşa harcanan bağlam.</p>
<p align="center">
  <a href="README.md">English</a> | <a href="README.zh.md">简体中文</a> | <a href="README.tr.md">Türkçe</a>
</p>
<p align="center">
  <a href="https://mtty.dev/miao">Web Sitesi</a> · <a href="#hızlı-başlangıç">Hızlı başlangıç</a> · <a href="#neden-miao">Neden miao</a> · <a href="docs/guide.en.md">Kılavuz</a> · <a href="https://github.com/mttydev/miao/releases">Sürümler</a> · <a href="#benzer-ve-ilgili-projeler">İlgili projeler</a>
</p>

---

**miao, terminalde sizinle birlikte çalışan açık kaynaklı bir yapay zeka ajanıdır.** Görünür ilerleme, bağlam kullanımı ve maliyet takibiyle kod yazmak, konuları araştırmak, dosyaları yönetmek ve görevleri otomatikleştirmek için seçtiğiniz modelleri kullanın.

Bütünleşik bir çalışma alanı için miao'yu yapay zeka yerel terminal ve düzenleyici olan [mtty](https://github.com/mttydev/mtty) içinde çalıştırın: bölme başına ajan durumunu görün, girdi gerektiğinde bildirim alın ve o çalışırken bir sonraki isteminizi sıraya koyun.

miao, modelin etrafındaki mühendisliğe odaklanır: **bağlam verimliliği, kalıcı oturumlar, iş birliği ve uzun süren işler üzerinde kontrol.** Amacı, daha az bekleme ve daha az boşa harcanan token ile aynı yararlı sonucu elde etmektir.

## Çalışırken görün

![Güncel miao terminal arayüzü: satır içi düzenleme diff'i, yanıt ve bağlam telemetrisi](docs/images/miao.png?v=20261004)

Aşağıdaki terminal ve tarayıcı ekran görüntüleri, bir model isteği olmadan gerçek uygulamaya aktarılan aynı **örnek oturumu** kullanır. 2026-10-04 tarihinde miao v0.1.0 ile yakalanmıştır; isteğe bağlı mini arayüz güncel v0.1.1 geliştirme derlemesindendir ve yerleşik demo modunu kullanır.

| Sağlayıcınızı seçin                                                                              | Tarayıcıda inceleyin                                                                                              |
| ------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------- |
| ![Sağlayıcı seçicide mevcut servislere göz atma](docs/images/miao-providers.gif?v=20261004) | ![Genişletilmiş bir kod diff'i ve taslak takip istemiyle tarayıcı çalışma alanı](docs/images/miao-web.png?v=20261004) |

| Kompakt etkileşimli mod · geliştirme önizlemesi                                                                  | Bir düzenlemeye izin vermeden önce inceleyin · geliştirme önizlemesi                                                                        |
| --------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| ![Görev ilerlemesi, bir düzenleme diff'i ve çoktan seçmeli bir soru içeren mini demo](docs/images/miao-mini.gif?v=20261004) | ![Diff'i ve Allow once, Allow always ve Reject seçeneklerini gösteren mini düzenleme izni istemi](docs/images/miao-permission.png?v=20261004) |

Daha fazla kısa, kontrol edilebilir demo: **[mtty.dev üzerinde miao](https://mtty.dev/miao#screens)**.

## Neden miao

### İşe uyan modeli seçin

Tek bir arayüzde birden fazla sağlayıcı kullanın, bir oturum içinde model değiştirin ve kendi modelleri ile izinlerine sahip uzman ajanlar yapılandırın. Sağlayıcı kataloğu ve özel sağlayıcı yapılandırması, iş akışınızı tek bir satıcı etrafında yeniden kurmak yerine akıl yürütme kalitesine, hıza veya fiyata göre seçim yapmanızı sağlar.

### Uzun görevleri sürdürün — ve ilerlerken yönlendirin

Ajan çalışırken yeni bir gereksinim ekleyin. V2, yürütmeyi zamanlamadan önce girdiyi kaydeder ve güvenli bir sağlayıcı turu sınırında konuşmaya dahil eder. Açıkça sıraya alınan girdiler, mevcut iş aksi halde boşta kalana kadar bekler. Başka bir konuşma başlatmadan veya her araç çağrısını kesintiye uğratmadan görevi iyileştirebilirsiniz.

### İş birliğine kendi bağlamını verin

Kapsamlı bir görevi uzman bir alt ajana devredin ve bu alt ajan oturumuna daha sonra devam edin. V2 ayrıca `list_sessions` ve `send_message` sağlar: ajanlar izinlere tabi olarak aynı projedeki diğer oturumları keşfedebilir ve bunlara mesaj gönderebilir. Ana oturum genel hedefi korurken araştırmalar ve odaklı incelemeler kendi konuşmalarında yaşayabilir.

### Token'larınızın ve paranızın nereye gittiğini görün

Tur başına kullanım, tahmini maliyet, ilk token'a kadar geçen süre ve istem önbelleği telemetrisi performansı görünür kılar. Bağlam Dönemleri (Context Epochs) değişmez bir sistem bağlamı temel çizgisini korur ve değişiklikleri kronolojik olarak kabul ederek önbellek ön eklerinin yeniden kullanılabilir kalmasına yardımcı olur. İsteğe bağlı çıktı budama, sıkıştırma ayarı, önbellek TTL'i ve oturum bütçeleri daha uzun görevler için size denetim sunar.

Maliyet tahminleri yapılandırılmış model tarifelerini ve sağlayıcı para birimi meta verilerini kullanır. Bir oturumu anlamak için yararlıdırlar; sağlayıcınızın faturalandırması esas olmaya devam eder.

### Devam edebileceğiniz ve inceleyebileceğiniz bir kayıt tutun

V2, kalıcı istem kabulü ve olay destekli bir oturum geçmişi kullanır. Her terminal penceresini tek kullanımlık bir sohbet gibi görmek yerine oturumları listeleyin, yeniden açın, çatallayın ve dışa aktarın. Büyük araç sonuçları model bağlamında sınırlandırılır ve mümkün olduğunda tam çıktı geçici dosyalarda saklanır.

Kalıcı geçmiş, bir çökme sonrasında otomatik yürütme kurtarması anlamına **gelmez**: tamamlanmamış sağlayıcı işleri açık bir devam ettirme gerektirir ve keyfi komutların tam olarak bir kez çalışacağı garanti edilmez.

## Günlük mühendislik için üretildi

| Neye ihtiyacınız var                  | miao ne sunar                                                                                               |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| Tanımadığınız bir depoyu anlamak      | Dosya okuma, arama, proje talimatları, beceriler ve uzman alt ajanlar                                       |
| Bir değişikliği uygulamak ve doğrulamak | Dosya düzenlemeleri ve yamalar, kabuk komutları, isteğe bağlı LSP tanılamaları ve biçimlendirme             |
| Araç değiştirmeden modelleri karşılaştırmak | Oturum içi model seçimi, sağlayıcı yapılandırması, desteklenen yerlerde ajan başına modeller ve akıl yürütme çeşitleri |
| Daha büyük bir görev üzerinde çalışmak | Yapılacaklar, görev ortası girdi, kalıcı oturumlar ve isteğe bağlı otonom devam döngüsü                    |
| Kendi araçlarınızı bağlamak           | Yerel ve uzak MCP sunucuları, özel komutlar, beceriler ve eklentiler                                        |
| İşi incelemek ve yeniden kullanmak    | Diff'ler, oturum çatalları, geçmiş dışa aktarma ve izin istemleri                                           |
| Başka bir uygulamayla bütünleşmek     | HTTP sunucusu, tarayıcı arayüzü, CLI otomasyonu ve çalışma alanında üretilen Promise / Effect istemcileri   |

**Somut bir görev deneyin:** “Bu başarısız olan testin nedenini bul, düzelt, ilgili kontrolleri çalıştır ve diff'i açıkla.” Ardından çalışırken bir kısıt ekleyin: “Genel API'yi koru ve yeni çalışma zamanı bağımlılıklarından kaçın.”

## Hızlı başlangıç

macOS veya Linux'ta bash betiğiyle ya da Windows'ta PowerShell üzerinden kurun:

```powershell
irm https://mtty.dev/miao/install.ps1 | iex   # Windows: PowerShell 5.1 veya 7
```

Windows terminal işlemesi hâlâ doğrulanmaktadır; bkz. [Windows doğrulama notları](docs/windows-vt-verification.en.md).

Windows ikili dosyaları şu anda imzasızdır.

```bash
curl -fsSL https://mtty.dev/miao/install | bash   # bu deponun kurulum betiğine yönlendirir

miao providers login          # bir sağlayıcı seçin ve hesabınızı / API anahtarınızı bağlayın
cd /path/to/project
miao                          # terminal arayüzünü başlatın
```

TUI içinde komut paleti için `ctrl+p` veya model seçimi için `ctrl+x m` kullanın. Mevcut modelleri incelemek için `miao models` çalıştırın.

```bash
miao run "Explain this repository's architecture and identify its main entry points"
miao web                      # tarayıcı arayüzünü açın
miao upgrade                  # sürüm ikili dosyasını güncelleyin
```

Yükleyici sürüm komutunu `~/.miao/bin/miao` konumuna yerleştirir. Yapılandırma, izinler, MCP, LSP ve sorun giderme için [kılavuz](docs/guide.en.md) ile başlayın.

## Sınırları olan daha uzun görevler

V2 otonom döngüsü isteğe bağlıdır. Yineleme ve duraksama korumalarıyla birlikte açık yapılacaklar kaldığı sürece devam eder. Bir bütçe maliyeti, token'ları, sağlayıcı turlarını veya gerçek zamanı sınırlar: bunu yapılandırmada Oturum için belirtin veya `goal` aracıyla bir Hedef için belirtin. Beşte dördü harcandığında çalışma kalan miktarla uyarılır ve limite ulaşıldığında bir sonraki sağlayıcı turu sınırında durur — önceden gönderdiğiniz mesaj bir sonraki çalışma için sırada kalır ve model asla değiştirilmez. Para yalnızca bir sağlayıcının bildirdiğini sayar; bu nedenle para iddiasında bulunan bir not bir turun ücretsiz olup olmadığını ve kalan tutarın bir alt sınır olduğunu belirtir. Bir ajanın kendi `budget` değeri farklı bir sınırdır: tek bir istem boyunca biriken sağlayıcı tarafından faturalandırılan token'ları sınırlar, bu yüzden uzun bir orkestrasyon istemi bağlam penceresi neredeyse boşken ona ulaşabilir. Her hatırlatıcı tetiklenen niceliği adlandırır ve harcanmış bir ödenek sayımın yanında pencerenin doluluğunu ve sıkıştırma eşiğini yazdırır. Bir sağlayıcı faturası veya halihazırda sürmekte olan bir tur üzerinde kesin bir üst sınır değildir.

```jsonc
{
  "loop": { "enabled": true, "max_iterations": 25 },
  "budget": { "cost_usd": 5, "turns": 40, "wallclock_ms": 3600000 },
  "compaction": { "prune": true },
}
```

`cost.budget_usd` aynı bütçenin para ekseni olarak desteklenmeye devam eder.

Bunu bir proje için `.miao/miao.jsonc` veya genel olarak `~/.config/miao/miao.jsonc` içine koyun. Bu ayarlar isteğe bağlıdır; bunları ayarlamadan önce [yapılandırma kılavuzuna](docs/guide.en.md#4-configuration) bakın.

## Uzaktan Kontrol

Kendinden barındırmalı bir aktarıcı yapılandırmak ve Uygulama/Web cihazlarını eşleştirmek veya iptal etmek için TUI içinde `/remote-control` kullanın. Eski WeChat/QQ bağlayıcıları ve yerel IM oturum Yönlendiricisi kaldırılmıştır. Bkz. [Çalışma zamanı davranışı](docs/runtime.md) ve [aktarıcı kurulumu](packages/engine-ts/remote-control/README.md).

## İnceleyebileceğiniz performans çalışması

miao, bu deponun önceki TypeScript uygulamasına karşı Rust hızlandırıcıları ve kıyaslamalar içerir. Aynı makinede sürüm derlemeleri ve medyanlar kullanılarak kaydedilen seçili ölçümler:

| Yalıtılmış işlem                               | TypeScript temel çizgisi | Rust yerel | Hızlanma |
| ---------------------------------------------- | ------------------------ | ---------- | -------- |
| Düzenleme bulanık eşleşmesi, 12 bin satır      | 0.76 ms                  | 0.39 ms    | 1.9×     |
| Yama Unicode normalleştirmesi, 20 bin satır    | 13.06 ms                 | 5.21 ms    | 2.5×     |
| Git durumu, 10 dosya                           | 12.3 ms                  | 1.0 ms     | 11.9×    |

Bunlar **bileşen kıyaslamalarıdır; uçtan uca görev hızlanmaları veya bugünkü yukarı akış sürümüyle karşılaştırmalar değildir**. Bunların arkasındaki temel yapılar V1 araçlarıyla birlikte gitmedi: V2'nin `edit` ve `apply_patch` araçları, eklenti yüklendiğinde (ki varsayılan olarak öyledir) aynı yerel eşleştirme ve türetmeyi çağırır ve `MIAO_NATIVE=0` TypeScript uygulamalarına geri döner. Eklenti ayrıca isteğe bağlı işletim sistemi korumalı alan çalıştırıcısını da destekler ve işlem içi Git durumu aynı bayrağın arkasına bağlıdır. Bkz. [kaydedilen bileşen kıyaslamaları](docs/native-benchmarks.en.md).

### Herkese açık temel çizgi: başlangıç, bellek, boşta maliyet, çökme kurtarma

2026-10-02 tarihinde 16 GB RAM'li bir Apple M2, macOS 26.5 üzerinde ölçülmüştür. Her çalıştırma derlenmiş TUI'ı boş bir git deposunda 160×45 tmux bölmesinde başlatır ve istem işlenene kadar bekler. İki derleme dönüşümlü olarak, her biri 6 başlangıç ve 2 iki dakikalık boşta çalışma şeklinde çalıştırılmıştır. Makinede başka ajanlar çalışıyordu (1 dakikalık yük ortalaması 3.5–12.4), bu nedenle mutlak sayıları bir üst sınır olarak değerlendirin; karşılaştırma birebirdir.

| Ölçüt                                      | 0.0.31 sürümü  | `main` @ `a230e1302` | Faz 0 hedefi |
| ------------------------------------------ | -------------- | -------------------- | ------------ |
| Başlatmadan isteme (6 çalıştırma medyanı) | 2.27 s         | 1.82 s               | < 2.5 s      |
| İsteme ulaşırken harcanan CPU süresi       | 3.3 s          | 2.4 s                | —            |
| İstem göründüğünde RSS                     | ~1,080 MB      | ~700 MB              | —            |
| 2 dakika boşta kaldıktan sonra RSS         | 1,240–1,380 MB | 853 MB               | < 600 MB     |
| Boşta iken CPU (1–2. dakikalar)            | 2.0–2.6 %      | 2.2–2.3 %            | < 1 %        |

Çökme kurtarma (0.0.31): bir `bash` araç çağrısı çalışırken işlemi `SIGKILL` ile sonlandırmak hiç mesaj kaybetmedi; `miao -c` oturumu yeniden açtı ve sonraki istem ona devam etti. Üç boşluk kalıyor: kesintiye uğrayan araç çağrısı bir sonraki tur başlayana kadar çalışıyor görünmeye devam ediyor, başlattığı alt işlem çalışmayı sürdürüyor ve hiçbir şey otomatik olarak devam etmiyor.

Boşta RSS ve boşta CPU hâlâ Faz 0 hedeflerinin gerisinde kalıyor; bkz. [specs/architecture.md](specs/architecture.md).

## Durum ve mimari

miao 1.0 öncesi aşamadadır. V1 oturum çalışma zamanı ve eski `/session/*` rotaları kaldırılmıştır, bu nedenle gönderilen her istemci tek V2 çekirdeğini çalıştırır. V2; Effect servisleri, Konum kapsamlı araçlar, kalıcı gelen kutuları, olay destekli geçmiş ve Bağlam Dönemleri kullanır. Yerel yürütme koordinasyonu işlem yerelindedir; kümelenmiş yürütme ve otomatik çökme devamı uygulanmamıştır.

| Yetenek                                                                        | Kullanılabilirlik                                                     |
| ------------------------------------------------------------------------------ | --------------------------------------------------------------------- |
| V2 oturumları, istem kabulü, bağlam dönemleri, proje yerelinde oturum mesajlaşması | Uygulandı                                                             |
| Otonom devam, maliyet bütçeleri, budama ve sıkıştırma ayarı                    | İsteğe bağlı; davranış ayara göre değişir                             |
| Kod Modu (`MIAO_EXPERIMENTAL_CODE_MODE=1`)                                     | Deneysel                                                              |
| bash için işletim sistemi korumalı alanı                                       | V2; `sandbox` yapılandırması veya `MIAO_SANDBOX=1` ile isteğe bağlı (macOS/Linux) |
| Eski veritabanı geçişi (`miao db backfill` / `compact` / `restore`)            | V2 öncesinde yazılan veritabanları için korundu                       |
| Üretilen istemciler ve gömülü Effect konağı                                    | Özel çalışma alanı paketleri; API hâlâ gelişiyor                      |

### V1'den V2'ye

V1, miao'nun opencode'dan devraldığı oturum çalışma zamanıydı; V2, miao'nun yeniden yazılmış çekirdeğidir. V1 oturum çalışma zamanı, eski araçları ve `/session/*`, `/permission/*`, `/question/*` ve `/sync/*` rotaları kaldırılmıştır; bu nedenle gönderilen tüm istemciler V2 çalıştırır. İki uyumluluk yüzeyi kalır: V2 öncesinde yazılmış geçmişi okuyan veritabanı geçiş katmanı ve hâlâ `/api/*` altına taşınmakta olan oturum dışı eski rotalar.

| Konu                                                         | Durum                                                      |
| ------------------------------------------------------------ | ---------------------------------------------------------- |
| Oturum yürütme, araçlar, izinler                             | Yalnızca V2                                                |
| JS SDK'daki `/session/*` rotaları ve eski oturum yöntemleri  | Artık sunucu tarafından sunulmuyor                         |
| Eski `message` / `part` tabloları                            | `miao db backfill` ile okunur; `miao db compact` ile silinir |
| Taşınabilir dışa ve içe aktarma (`miao export` / `import`)   | Korundu                                                    |
| Eski biçimde yazılmış yapılandırma                           | V2 yapılandırma yükleyicisi tarafından hâlâ okunur         |
| Oturum dışı eski rotalar (`/config`, `/mcp`, `/lsp`, …)      | Hâlâ sunuluyor; `/api/*` geçişi sürüyor                    |

Bkz. [specs/v2/v1-retirement.md](specs/v2/v1-retirement.md) ve [specs/architecture.md](specs/architecture.md).

Sürümler için `miao`, kaynaktan yineleme için `miao-dev` ve derlenmiş kopya doğrulaması için `miao-preview` kullanın. Yeni kaynak özellikleri kurulu sürümde henüz bulunmayabilir.

## Benzer ve ilgili projeler

### Yerel varsayılanlar ve isteğe bağlı servisler

miao, bir miao veya OpenCode hesabı olmadan yerel olarak çalışır. Seçtiğiniz model sağlayıcısını API anahtarı veya desteklenen OAuth yöntemiyle doğrudan bağlayın.

- Model meta verileri yalnızca mtty.dev adresindeki miao kataloğundan gelir. Web sitesi bunu herkese açık [models.dev](https://models.dev) verilerinden ve miao tarafından sürdürülen sağlayıcılardan (şu anda Command Code) oluşturur; çalışma zamanı models.dev'e geri dönmez. Derlemeler çevrimdışı başlangıç için bir anlık görüntü paketler. `MIAO_MODELS_URL` ile bir kaynak sabitleyin veya `MIAO_MODELS_PATH` ile kesin bir katalog seçin.
- [Command Code](https://commandcode.ai) bir abonelik sağlayıcısı olarak kullanılabilir: `miao auth login commandcode` (tarayıcı destekli) veya `CMD_API_KEY` ile bağlayın; modelleri çalışma zamanında hesaptan keşfedilir.
- Sürüm notları ve güncellemeler [mttydev/miao releases](https://github.com/mttydev/miao/releases) sayfasından gelir.
- Oturum paylaşımı ve paylaşım URL'si içe aktarmaları kaldırılmıştır. Taşınabilir oturum dosyaları için `miao export` / `miao import` kullanın; eski `enterprise.url` yapılandırması paylaşımı etkinleştirmez.
- MIAO'nun Console hesabı veya kuruluş desteği yoktur. `MIAO_CONSOLE_URL` yalnızca OpenCode sağlayıcı entegrasyonu için Console OAuth'u açar; özel bir sunucu `MIAO_CONSOLE_CLIENT_ID` belirtebilir.
- OpenCode Zen / Go, gerçek OpenCode servis adları ve uç noktalarıyla isteğe bağlı üçüncü taraf model sağlayıcıları olarak kalır.

| Proje                      | Nedir                                                                                                                              | Bağlantılar                                                                                                                     |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| **miao** (bu depo)         | Terminalde sizinle çalışan yapay zeka ajanı                                                                                        | [mtty.dev/miao](https://mtty.dev/miao) · [docs](https://mtty.dev/docs/miao) · [mttydev/miao](https://github.com/mttydev/miao) |
| **mtty**                   | Bir bölmedeki hangi ajanın çalıştığını, sizi beklediğini veya bittiğini gösteren Rust ile yazılmış GPU işlenmiş terminal (macOS, Linux, Windows) | [mtty.dev/mtty](https://mtty.dev/mtty) · [mttydev/mtty](https://github.com/mttydev/mtty)                                      |
| **mtty.dev**               | Her ikisi için web sitesi ve belgeler                                                                                              | [mtty.dev](https://mtty.dev)                                                                                                    |

miao ve mtty ayrı projelerdir ve her biri diğeri olmadan çalışır. miao'yu bir mtty bölmesi içinde çalıştırdığınızda durumunu (çalışıyor, sizi bekliyor, bitti, hata) mtty'ye bildirir; mtty de bölmeye rozet ekler, ajan size ihtiyaç duyduğunda sizi bilgilendirir ve boşta kaldığında sıradaki isteminizi gönderir. mtty dışında bu bildirim hiçbir şey yapmaz. `miaotty` bu terminalin kişisel bir macOS prototipiydi ve yerini mtty'ye bırakmıştır.

## Belgeler ve geliştirme

- [Kullanım kılavuzu](docs/guide.en.md) · [使用指南](docs/guide.zh.md)
- [Sağlayıcılar ve modeller](docs/providers.en.md) · [Uzaktan Kontrol](docs/remote-control.en.md)
- [Neden miao'yu seçmeli?](docs/why-miao.en.md) — kullanım senaryoları, güncel durum ve yönelim
- [Açık kaynak ajan karşılaştırması](docs/agent-comparison.en.md) — sekiz iş akışı, kaynak kanıtı ve sınırlar
- [Sürüm iş akışı](docs/release.en.md) — sürümler, derlemeler ve yayımlama
- [Çalışma zamanı tasarımı](CONTEXT.md) · [V2 belirtimleri](specs/v2) — oturum, bağlam ve istemci sözleşmeleri

Geliştirme için [Bun](https://bun.sh) gerektirir:

```bash
bun install
bun run dev
# Değiştirdiğiniz pakette kontrolleri çalıştırın, örneğin:
cd packages/miao
bun typecheck
```

## Teşekkürler ve lisans

miao, MIT lisanslı kodu bu projenin önemli bir bölümünü sağlayan [opencode](https://github.com/anomalyco/opencode) projesinden türetilmiştir. Bu temel için opencode yazarlarına ve katkıda bulunanlarına teşekkür ederiz. miao, kendi geliştirme yönü ve sürümleriyle ayrı bir proje olarak sürdürülmektedir.

miao, MIT Lisansı altında yayımlanmıştır. [LICENSE](LICENSE) hem miao yazarlarının hem de opencode'un telif hakkı bildirimlerini ve MIT izin bildirimini korur. Yazılımın kopyaları veya önemli bölümleriyle birlikte bu bildirimleri koruyun. Üçüncü taraf bileşenler kendi lisanslarını ve bildirimlerini saklı tutar.

Kaynak atfı ve isteğe bağlı servislerle ilişkisi için [proje kökenleri ve lisanslama](docs/attribution.en.md) belgesine bakın.
