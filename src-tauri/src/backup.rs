//! Profil Yedekleme ve Geri Yükleme (.zip arşivi) modülü.
//!
//! `%APPDATA%\Ozan\library.json` (tüm ses meta verileri, renkler, kazançlar,
//! klasör ilişkileri, kısayol/passthrough yapılandırmaları, Panic/PTT ayarları)
//! ve `%APPDATA%\Ozan\sounds\` altındaki tüm ses dosyalarını standart `.zip`
//! arşivi olarak dışa aktarır ve geri yükler.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::mem::{size_of, zeroed};
use std::path::{Component, Path, PathBuf};

use crc32fast::Hasher;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use flate2::Compression;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, GetSaveFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
    OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};

use crate::library::{is_supported, LibraryData};

const LOCAL_FILE_HEADER_SIG: u32 = 0x0403_4b50;
const CENTRAL_DIR_HEADER_SIG: u32 = 0x0201_4b50;
const END_OF_CENTRAL_DIR_SIG: u32 = 0x0605_4b50;
const FLAG_UTF8: u16 = 1 << 11; // Bit 11: UTF-8 dosya adları

struct CentralEntry {
    name: Vec<u8>,
    method: u16,
    crc32: u32,
    comp_size: u32,
    uncomp_size: u32,
    header_offset: u32,
}

/// Windows yerel "Farklı Kaydet" penceresiyle `.zip` yedekleme yolu seçtirir.
pub fn pick_save_zip_path() -> Option<PathBuf> {
    unsafe {
        let filter: Vec<u16> = "Ozan Profil Yedeği (*.zip)\0*.zip\0\0".encode_utf16().collect();
        let title: Vec<u16> = "Profil Yedeğini Kaydet (.zip)\0".encode_utf16().collect();
        let def_ext: Vec<u16> = "zip\0".encode_utf16().collect();
        let mut file_buf = vec![0u16; 1024];
        for (i, ch) in "ozan-profil-yedek.zip".encode_utf16().enumerate() {
            file_buf[i] = ch;
        }

        let mut ofn: OPENFILENAMEW = zeroed();
        ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = file_buf.as_mut_ptr();
        ofn.nMaxFile = file_buf.len() as u32;
        ofn.lpstrTitle = title.as_ptr();
        ofn.lpstrDefExt = def_ext.as_ptr();
        ofn.Flags = OFN_EXPLORER | OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

        if GetSaveFileNameW(&mut ofn) != 0 {
            let len = file_buf.iter().position(|&c| c == 0).unwrap_or(file_buf.len());
            Some(PathBuf::from(String::from_utf16_lossy(&file_buf[..len])))
        } else {
            None
        }
    }
}

/// Windows yerel "Dosya Aç" penceresiyle geri yüklenecek `.zip` arşivini seçtirir.
pub fn pick_open_zip_path() -> Option<PathBuf> {
    unsafe {
        let filter: Vec<u16> = "Ozan Profil Yedeği (*.zip)\0*.zip\0\0".encode_utf16().collect();
        let title: Vec<u16> = "Geri Yüklenecek Profil Yedeğini Seç (.zip)\0".encode_utf16().collect();
        let mut file_buf = vec![0u16; 1024];

        let mut ofn: OPENFILENAMEW = zeroed();
        ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = file_buf.as_mut_ptr();
        ofn.nMaxFile = file_buf.len() as u32;
        ofn.lpstrTitle = title.as_ptr();
        ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

        if GetOpenFileNameW(&mut ofn) != 0 {
            let len = file_buf.iter().position(|&c| c == 0).unwrap_or(file_buf.len());
            Some(PathBuf::from(String::from_utf16_lossy(&file_buf[..len])))
        } else {
            None
        }
    }
}

/// Tüm profil verilerini (`library.json` ve `sounds/` içindeki ses dosyalarını) `.zip` olarak dışa aktarır.
pub fn export_zip(data: &LibraryData, sounds_dir: &Path, zip_path: &Path) -> Result<usize, String> {
    let out_file = File::create(zip_path).map_err(|e| format!("Yedek dosyası oluşturulamadı: {e}"))?;
    let mut w = BufWriter::with_capacity(64 * 1024, out_file);
    let mut entries: Vec<CentralEntry> = Vec::new();

    // 1. library.json (Deflate ile sıkıştırılmış)
    let json_bytes = serde_json::to_vec_pretty(data).map_err(|e| e.to_string())?;
    write_entry_bytes(&mut w, &mut entries, "library.json", &json_bytes, true)?;

    // 2. sounds/ altındaki ses dosyaları (zaten sıkıştırılmış formatlar olduğundan hızlı akışla ekle)
    let mut sound_count = 0usize;
    let mut seen_files = std::collections::HashSet::new();
    for s in &data.sounds {
        if !seen_files.insert(s.file.to_lowercase()) {
            continue;
        }
        let path = sounds_dir.join(&s.file);
        if path.is_file() {
            let zip_name = format!("sounds/{}", s.file.replace('\\', "/"));
            write_entry_file(&mut w, &mut entries, &zip_name, &path)?;
            sound_count += 1;
        }
    }

    // 3. Central Directory ve End of Central Directory (EOCD) yaz
    let cd_offset = w.stream_position().map_err(|e| e.to_string())? as u32;
    for e in &entries {
        w.write_all(&CENTRAL_DIR_HEADER_SIG.to_le_bytes()).map_err(|e| e.to_string())?;
        w.write_all(&20u16.to_le_bytes()).map_err(|e| e.to_string())?; // version made by
        w.write_all(&20u16.to_le_bytes()).map_err(|e| e.to_string())?; // version needed
        w.write_all(&FLAG_UTF8.to_le_bytes()).map_err(|e| e.to_string())?;
        w.write_all(&e.method.to_le_bytes()).map_err(|e| e.to_string())?;
        w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // mod time
        w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // mod date
        w.write_all(&e.crc32.to_le_bytes()).map_err(|e| e.to_string())?;
        w.write_all(&e.comp_size.to_le_bytes()).map_err(|e| e.to_string())?;
        w.write_all(&e.uncomp_size.to_le_bytes()).map_err(|e| e.to_string())?;
        w.write_all(&(e.name.len() as u16).to_le_bytes()).map_err(|e| e.to_string())?;
        w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // extra len
        w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // comment len
        w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // disk start
        w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // internal attr
        w.write_all(&0u32.to_le_bytes()).map_err(|e| e.to_string())?; // external attr
        w.write_all(&e.header_offset.to_le_bytes()).map_err(|e| e.to_string())?;
        w.write_all(&e.name).map_err(|e| e.to_string())?;
    }
    let cd_end = w.stream_position().map_err(|e| e.to_string())? as u32;
    let cd_size = cd_end.saturating_sub(cd_offset);
    let count = entries.len().min(u16::MAX as usize) as u16;

    w.write_all(&END_OF_CENTRAL_DIR_SIG.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // disk num
    w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // start disk
    w.write_all(&count.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&count.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&cd_size.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&cd_offset.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // comment len
    w.flush().map_err(|e| e.to_string())?;

    Ok(sound_count)
}

fn write_entry_bytes<W: Write + Seek>(
    w: &mut W,
    entries: &mut Vec<CentralEntry>,
    name: &str,
    data: &[u8],
    deflate: bool,
) -> Result<(), String> {
    let header_offset = w.stream_position().map_err(|e| e.to_string())? as u32;
    let mut hasher = Hasher::new();
    hasher.update(data);
    let crc32 = hasher.finalize();
    let uncomp_size = data.len() as u32;

    let (method, payload) = if deflate {
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data).map_err(|e| e.to_string())?;
        let comp = enc.finish().map_err(|e| e.to_string())?;
        (8u16, comp)
    } else {
        (0u16, data.to_vec())
    };
    let comp_size = payload.len() as u32;
    let name_bytes = name.as_bytes().to_vec();

    write_local_header(w, &name_bytes, method, crc32, comp_size, uncomp_size)?;
    w.write_all(&payload).map_err(|e| e.to_string())?;

    entries.push(CentralEntry {
        name: name_bytes,
        method,
        crc32,
        comp_size,
        uncomp_size,
        header_offset,
    });
    Ok(())
}

fn write_entry_file<W: Write + Seek>(
    w: &mut W,
    entries: &mut Vec<CentralEntry>,
    name: &str,
    src_path: &Path,
) -> Result<(), String> {
    let header_offset = w.stream_position().map_err(|e| e.to_string())? as u32;
    let name_bytes = name.as_bytes().to_vec();
    let method = 0u16; // Stored (ses dosyaları zaten sıkıştırılmış veya hızlı akış için)

    // Yer tutucu başlık yaz, akış sonunda CRC32 ve boyutu geri dönüp güncelle
    write_local_header(w, &name_bytes, method, 0, 0, 0)?;

    let mut f = File::open(src_path).map_err(|e| format!("Ses dosyası okunamadı: {e}"))?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut hasher = Hasher::new();
    let mut total: u64 = 0;
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        w.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        total += n as u64;
    }
    let end_pos = w.stream_position().map_err(|e| e.to_string())?;
    let crc32 = hasher.finalize();
    let size = total.min(u32::MAX as u64) as u32;

    w.seek(SeekFrom::Start(header_offset as u64)).map_err(|e| e.to_string())?;
    write_local_header(w, &name_bytes, method, crc32, size, size)?;
    w.seek(SeekFrom::Start(end_pos)).map_err(|e| e.to_string())?;

    entries.push(CentralEntry {
        name: name_bytes,
        method,
        crc32,
        comp_size: size,
        uncomp_size: size,
        header_offset,
    });
    Ok(())
}

fn write_local_header<W: Write>(
    w: &mut W,
    name: &[u8],
    method: u16,
    crc32: u32,
    comp_size: u32,
    uncomp_size: u32,
) -> Result<(), String> {
    w.write_all(&LOCAL_FILE_HEADER_SIG.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&20u16.to_le_bytes()).map_err(|e| e.to_string())?; // version needed
    w.write_all(&FLAG_UTF8.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&method.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // mod time
    w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // mod date
    w.write_all(&crc32.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&comp_size.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&uncomp_size.to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&(name.len() as u16).to_le_bytes()).map_err(|e| e.to_string())?;
    w.write_all(&0u16.to_le_bytes()).map_err(|e| e.to_string())?; // extra len
    w.write_all(name).map_err(|e| e.to_string())?;
    Ok(())
}

/// `.zip` arşivini okuyarak `sounds/` dosyalarını ve `library.json` verisini geri yükler.
pub fn import_zip(sounds_dir: &Path, zip_path: &Path) -> Result<Option<LibraryData>, String> {
    let f = File::open(zip_path).map_err(|e| format!("Yedek dosyası açılamadı: {e}"))?;
    let file_len = f.metadata().map_err(|e| e.to_string())?.len();
    let mut r = BufReader::with_capacity(64 * 1024, f);

    // EOCD'yi dosyanın son 65 KiB'lık kısmında bul
    let tail_len = file_len.min(65557) as usize;
    r.seek(SeekFrom::End(-(tail_len as i64))).map_err(|e| e.to_string())?;
    let mut tail = vec![0u8; tail_len];
    r.read_exact(&mut tail).map_err(|e| format!("ZIP sonu okunamadı: {e}"))?;

    let eocd_pos = (0..=tail.len().saturating_sub(22))
        .rev()
        .find(|&i| u32::from_le_bytes([tail[i], tail[i + 1], tail[i + 2], tail[i + 3]]) == END_OF_CENTRAL_DIR_SIG)
        .ok_or_else(|| "Geçerli bir .zip arşivi değil (EOCD bulunamadı).".to_string())?;

    let total_entries = u16::from_le_bytes([tail[eocd_pos + 10], tail[eocd_pos + 11]]) as usize;
    let cd_offset = u32::from_le_bytes([
        tail[eocd_pos + 16],
        tail[eocd_pos + 17],
        tail[eocd_pos + 18],
        tail[eocd_pos + 19],
    ]) as u64;

    r.seek(SeekFrom::Start(cd_offset)).map_err(|e| e.to_string())?;
    let mut entries = Vec::with_capacity(total_entries);
    for _ in 0..total_entries {
        let mut hdr = [0u8; 46];
        r.read_exact(&mut hdr).map_err(|e| format!("Merkezi dizin okunamadı: {e}"))?;
        let sig = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        if sig != CENTRAL_DIR_HEADER_SIG {
            return Err("Bozuk .zip merkezi dizin başlığı.".into());
        }
        let method = u16::from_le_bytes([hdr[10], hdr[11]]);
        let crc32 = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
        let comp_size = u32::from_le_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]);
        let uncomp_size = u32::from_le_bytes([hdr[24], hdr[25], hdr[26], hdr[27]]);
        let name_len = u16::from_le_bytes([hdr[28], hdr[29]]) as usize;
        let extra_len = u16::from_le_bytes([hdr[30], hdr[31]]) as usize;
        let comment_len = u16::from_le_bytes([hdr[32], hdr[33]]) as usize;
        let header_offset = u32::from_le_bytes([hdr[42], hdr[43], hdr[44], hdr[45]]);

        let mut name = vec![0u8; name_len];
        r.read_exact(&mut name).map_err(|e| e.to_string())?;
        if extra_len + comment_len > 0 {
            r.seek(SeekFrom::Current((extra_len + comment_len) as i64))
                .map_err(|e| e.to_string())?;
        }
        entries.push(CentralEntry {
            name,
            method,
            crc32,
            comp_size,
            uncomp_size,
            header_offset,
        });
    }

    fs::create_dir_all(sounds_dir).map_err(|e| e.to_string())?;
    let mut restored_lib: Option<LibraryData> = None;

    for e in entries {
        let entry_name = String::from_utf8_lossy(&e.name).replace('\\', "/");
        if entry_name.ends_with('/') {
            continue;
        }
        r.seek(SeekFrom::Start(e.header_offset as u64)).map_err(|e| e.to_string())?;
        let mut lhdr = [0u8; 30];
        r.read_exact(&mut lhdr).map_err(|e| e.to_string())?;
        let lsig = u32::from_le_bytes([lhdr[0], lhdr[1], lhdr[2], lhdr[3]]);
        if lsig != LOCAL_FILE_HEADER_SIG {
            return Err("Bozuk .zip yerel dosya başlığı.".into());
        }
        let lname_len = u16::from_le_bytes([lhdr[26], lhdr[27]]) as i64;
        let lextra_len = u16::from_le_bytes([lhdr[28], lhdr[29]]) as i64;
        r.seek(SeekFrom::Current(lname_len + lextra_len)).map_err(|e| e.to_string())?;

        if entry_name.eq_ignore_ascii_case("library.json") {
            let mut raw_bytes = Vec::with_capacity(e.uncomp_size as usize);
            read_entry_stream((&mut r).take(e.comp_size as u64), e.method, e.crc32, &mut raw_bytes)?;
            let parsed: LibraryData =
                serde_json::from_slice(&raw_bytes).map_err(|err| format!("library.json geçersiz: {err}"))?;
            restored_lib = Some(parsed);
        } else if let Some(rel) = entry_name.strip_prefix("sounds/") {
            // Path traversal koruması: yalnızca tek bileşenli güvenli dosya adlarını kabul et
            let rel_path = Path::new(rel);
            if rel_path.components().count() != 1 || !matches!(rel_path.components().next(), Some(Component::Normal(_)))
            {
                continue;
            }
            if !is_supported(rel_path) {
                continue;
            }
            let dest = sounds_dir.join(rel_path);
            let out_f = File::create(&dest).map_err(|err| format!("Dosya çıkarılamadı ({rel}): {err}"))?;
            let mut bw = BufWriter::with_capacity(64 * 1024, out_f);
            read_entry_stream((&mut r).take(e.comp_size as u64), e.method, e.crc32, &mut bw)?;
            bw.flush().map_err(|err| err.to_string())?;
        }
    }

    Ok(restored_lib)
}

fn read_entry_stream<R: Read, W: Write>(mut src: R, method: u16, expected_crc: u32, dst: &mut W) -> Result<(), String> {
    let mut hasher = Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    match method {
        0 => loop {
            let n = src.read(&mut buf).map_err(|e| format!("ZIP okuma hatası: {e}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            dst.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        },
        8 => {
            let mut dec = DeflateDecoder::new(src);
            loop {
                let n = dec.read(&mut buf).map_err(|e| format!("ZIP açma hatası (Deflate): {e}"))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                dst.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            }
        }
        other => return Err(format!("Desteklenmeyen ZIP sıkıştırma yöntemi: {other}")),
    }
    if expected_crc != 0 && hasher.finalize() != expected_crc {
        return Err("ZIP arşivindeki dosyada CRC32 bütünlük hatası tespit edildi.".into());
    }
    Ok(())
}
