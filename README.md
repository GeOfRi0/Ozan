# Ozan — Ultra Hafif Windows Soundboard & Doğrudan Mikrofon APO Enjektörü

**Ozan** (`ozan.exe` + `ozan-apo.dll`), harici sanal ses sürücüsü (VB-Cable vb.) kurulumuna ihtiyaç duymadan doğrudan aktif fiziksel mikrofona gerçek zamanlı ses enjekte eden, Rust ve Tauri v2 ile geliştirilmiş yüksek performanslı bir Windows ses paneli (soundboard) uygulamasıdır.

Arka planda (System Tray) çalışırken **< 25 MB RAM (~3 MB Private)** ve **%0–1 altı CPU** tüketimi hedefler.

---

## Temel Teknik Özellikler

### 1. Windows User-Mode APO (`ozan-apo.dll`) & APO Chaining
- **Sürücüsüz Doğrudan Mikrofon Enjeksiyonu**: Saf Rust ile yazılmış `ozan-apo.dll` (`CLSID: {8A4E7B21-6C3D-4F92-B8E1-5D2A9F0C4E73}`) bileşeni, Windows Ses Motoru (`audiodg.exe`) içinde User-Mode Audio Processing Object (`IAudioProcessingObject`, `IAudioProcessingObjectRT`, `IAudioProcessingObjectConfiguration`, `IAudioSystemEffects`, `IAudioSystemEffects2`) olarak çalışır.
- **Üretici Efektlerini Koruyan APO Chaining**: Aktif mikrofonun Windows Kayıt Defteri (`FxProperties`) altındaki mevcut üretici ses efektleri (gürültü engelleme, beamforming, AGC, EQ vb.) **silinmez**. Orijinal CLSID değerleri yedeklenir, `ozan-apo.dll` efekt zincirine eklenir ve ses verisi işlendikten sonra orijinal APO zincirine iletilir.

### 2. Kilitsiz Paylaşımlı Bellek (IPC) & Gerçek Zamanlı Ses İşleme
- **Named Shared Memory & Lock-Free SPSC Ring Buffer**: `ozan.exe` ile `audiodg.exe` içindeki `ozan-apo.dll` arasında `Global\OzanAudioSharedMem_v1` (`Local\OzanAudioSharedMem_v1` yedekli) paylaşımlı belleği ve 64 bayt önbellek satırı hizalı (`#[repr(C, align(64))]`) kilitsiz SPSC halka tampon üzerinden ses verisi aktarılır.
- **Sıfır Bellek Ayırma / Sıfır Kilit (`APOProcess`)**: Gerçek zamanlı ses işleme döngüsü (`APOProcess`) içinde kesinlikle heap bellek ayırma (`Box`, `Vec`, `String`), kilit (`Mutex`) veya disk/kayıt defteri okuma işlemi yapılmaz.
- **`rubato` ile Gerçek Zamanlı Yeniden Örnekleme ve Kanal Eşitleme**: Farklı mikrofon örnekleme hızları (`44.1 kHz`, `48 kHz`, `96 kHz`, **Bluetooth `16 kHz` Mono** vb.) için `rubato` kütüphanesinin sıfır bellek ayırmalı `process_into_buffer` arayüzüyle gerçek zamanlı yeniden örnekleme ve Stereo/Mono kanal eşitlemesi uygulanır.
- **`IMMNotificationClient` Tak-Çıkar (Hotplug) Algılama**: USB veya Bluetooth mikrofonların takılıp çıkarılması ve varsayılan mikrofon değişimleri Windows Core Audio `IMMNotificationClient` arayüzüyle anında algılanır ve yeni cihaza otomatik bağlanılır.

### 3. Diskten Akış (Streaming Decoder) ve Çoklu Format Desteği
- **`.opus`, `.mp3`, `.wav`, `.ogg`, `.flac` Desteği**: Sürükle-bırak ile eklenen tüm ses dosyaları `%APPDATA%\Ozan\sounds\` dizinine kopyalanır ve isteğe bağlı olarak `-1 dBFS` tepe seviyesine normalize edilir.
- **Toplu RAM Yüklemesi Yok**: Ses dosyaları belleğe topluca yüklenmez; oynatma sırasında ayrı bir `ozan-decoder` iş parçacığı tarafından diskten küçük parçalar halinde okunur. Süre üst sınırı yoktur.
- **5 Eşzamanlı Ses & FIFO Hard-Cut**: Aynı anda en fazla 5 ses ducking uygulanmadan çalabilir; 6. ses tetiklendiğinde en eski başlatılan ses anında kesilir (`FIFO hard-cut`). Her kart için **"Baştan Başlat"** veya **"Üst Üste Bindir"** tekrar modu seçilebilir.

### 4. Self-Healing (Kendini Onarma) ve Fallback (Doğrudan Çıkış) Modu
- **SHA-256 ve `FxProperties` Denetimi**: Her açılışta `ozan-apo.dll` dosyasının SHA-256 bütünlüğü, COM kaydı ve Windows Kayıt Defteri `FxProperties` APO zinciri otomatik olarak denetlenir.
- **Tek Tıkla Otomatik Onarım**: Windows güncellemesi veya dış müdahale ile kayıtlar sıfırlanmışsa arayüzden tek tıkla otomatik onarım sunulur.
- **Çökmeyen Fallback Modu**: Güvenlik yazılımı (EDR/Antivirüs) veya sistem politikası DLL yüklenmesini engellerse uygulama kesinlikle çökmez veya kilitlenmez; otomatik olarak **"Fallback (Doğrudan Çıkış)"** moduna geçer ve durumu arayüzde bildirir.

### 5. Kısayol Yakalama, Kırpma, Bas-Konuş ve Yedekleme
- **Sınırsız Fare / MMO ve Klavye Kısayolları**: `WH_KEYBOARD_LL`, `WH_MOUSE_LL` ve Windows Raw Input / HID (`WM_INPUT`) ile klavye tuşları, `XBUTTON1`/`XBUTTON2` ve çok tuşlu MMO farelerinin tüm ek tuşları yakalanır; her kısayol için **Passthrough** seçeneği ve varsayılan **`End` (`VK_END`) Panic** tuşu bulunur.
- **Ses Kırpma Modülü**: Her ses kartı için dalga formu üzerinde başlangıç/bitiş seçimi, **"1 Kerelik Çal"** ve **"Kırpılmış Şekilde Kaydet"** özellikleri mevcuttur.
- **Bas-Konuş (Push-to-Talk) & Fiziksel Mikrofon Susturma**: Ses çaldığı sürece arka planda `SendInput` ile Bas-Konuş tuşuna basılı tutma ve efekt süresince fiziksel mikrofonu geçici olarak susturma seçenekleri yer alır.
- **Profil Yedekle / Geri Yükle (`.zip`)**: Tüm ses dosyaları, klasör ilişkileri ve kısayol ayarları tek bir `.zip` arşivi olarak dışa/içe aktarılabilir.

---

## Kurulum ve Derleme Adımları

### Gereksinimler
- **İşletim Sistemi**: Windows 10 / 11 (x64)
- **Rust**: `1.77+` (`x86_64-pc-windows-msvc`)
- **C++ Derleme Araçları**: MSVC Build Tools + Windows 10/11 SDK
- **Node.js**: `20+`

### Geliştirme ve Release Derlemesi

1. Bağımlılıkları yükleyin ve ön yüzü derleyin:
   ```powershell
   npm install
   npm run build
   ```

2. Rust çalışma alanını (`ozan.exe` ve `ozan-apo.dll`) release modunda derleyin:
   ```powershell
   cd src-tauri
   cargo build --release --workspace
   Copy-Item -Force "target/release/ozan_apo.dll" "target/release/ozan-apo.dll"
   ```

3. Tauri CLI ile uygulama paketini oluşturun:
   ```powershell
   npx tauri build
   ```

4. Derleme çıktıları:
   - Ana Uygulama: `src-tauri/target/release/ozan.exe`
   - User-Mode APO Eklentisi: `src-tauri/target/release/ozan-apo.dll`

---

## SignPath.io Kod İmzalama Süreci

Bu proje, GitHub Actions ([.github/workflows/release.yml](.github/workflows/release.yml)) ve [signpath.yml](signpath.yml) yapılandırması ile **SignPath.io** açık kaynak kod imzalama sürecine uyumludur. `v*` etiketli sürümlerde `ozan.exe` ve `ozan-apo.dll` temiz ve ayrı olarak derlenir, SHA-256 özetleri üretilir ve Authenticode imzalaması için SignPath.io'ya iletilir.

## Lisans

Bu proje [MIT Lisansı](LICENSE) altında lisanslanmıştır.
