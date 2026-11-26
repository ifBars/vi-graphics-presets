use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;

use serde_json::{json, Value};
use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT,
    MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::{parse_preset, plan, Platform, Preset, RawRow};

const ORIGINAL_HASH: &[u8; 64] =
    b"b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93";
const CALL_RVA: usize = 0x0577_c8a3;
const PARSER_RVA: usize = 0x015e_0750;
const PROFILE_RVA: usize = 0x0501_acf0;
const TABLE_RVA: usize = 0x09aa_53c0;
const LOADED_RVA: usize = 0x09aa_53b8;
const ORIGINAL_CALL: [u8; 5] = [0xe8, 0xa8, 0x3e, 0xe6, 0xfb];
const ROW: usize = 8;
const MAX_JSON: u64 = 64 * 1024;
const MAX_PATH_UNITS: usize = 32768;

type Parser = unsafe extern "system" fn(*const u8) -> u8;
type ProfileGetter = unsafe extern "system" fn() -> *const u8;
type Fallible<T> = Result<T, String>;

struct Installed {
    base: usize,
    length: usize,
    preset: Preset,
    log: PathBuf,
}

static INSTALLED: Mutex<Option<Installed>> = Mutex::new(None);

#[derive(Clone, Copy)]
enum Access {
    Read,
    Write,
    Execute,
}

fn region(address: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    let mut info = MEMORY_BASIC_INFORMATION::default();
    let size = unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut info,
            std::mem::size_of_val(&info),
        )
    };
    (size == std::mem::size_of_val(&info)).then_some(info)
}

fn accessible(address: usize, length: usize, access: Access) -> bool {
    let Some(end) = address.checked_add(length) else {
        return false;
    };
    if address == 0 || length == 0 {
        return false;
    }
    let mut cursor = address;
    while cursor < end {
        let Some(info) = region(cursor) else {
            return false;
        };
        if info.State != MEM_COMMIT || info.Protect & PAGE_GUARD != 0 {
            return false;
        }
        let valid = match access {
            Access::Read => matches!(
                info.Protect & 0xff,
                PAGE_READONLY
                    | PAGE_READWRITE
                    | PAGE_WRITECOPY
                    | PAGE_EXECUTE_READ
                    | PAGE_EXECUTE_READWRITE
                    | PAGE_EXECUTE_WRITECOPY
            ),
            Access::Write => matches!(
                info.Protect & 0xff,
                PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
            ),
            Access::Execute => matches!(
                info.Protect & 0xff,
                PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
            ),
        };
        let Some(next) = (info.BaseAddress as usize).checked_add(info.RegionSize) else {
            return false;
        };
        if !valid || next <= cursor {
            return false;
        }
        cursor = next;
    }
    true
}

fn image_address(base: usize, image_size: usize, rva: usize, bytes: usize) -> Fallible<usize> {
    if rva.checked_add(bytes).is_none_or(|end| end > image_size) {
        return Err(format!("RVA {rva:#x} exceeds mapped image"));
    }
    base.checked_add(rva)
        .filter(|address| address.checked_add(bytes).is_some())
        .ok_or_else(|| "loaded image address overflow".into())
}

unsafe fn read<T: Copy>(address: usize) -> T {
    unsafe { ptr::read_unaligned(address as *const T) }
}

fn write_bits(address: usize, bits: u32) {
    // The parser and caller are the synchronous owner here. The table uses
    // aligned eight-byte rows; volatile writes also make verification explicit.
    unsafe { ptr::write_volatile(address as *mut u32, bits) };
}

fn read_bits(address: usize) -> u32 {
    unsafe { ptr::read_volatile(address as *const u32) }
}

fn wide_path(pointer: *const u16) -> Fallible<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let address = pointer as usize;
    let mut units = Vec::new();
    for index in 0..MAX_PATH_UNITS {
        let at = address
            .checked_add(index * 2)
            .ok_or("path pointer overflow")?;
        if !accessible(at, 2, Access::Read) {
            return Err("unreadable UTF-16 path".into());
        }
        let unit = unsafe { read::<u16>(at) };
        if unit == 0 {
            let path = PathBuf::from(std::ffi::OsString::from_wide(&units));
            if !path.is_absolute() {
                return Err("path must be absolute".into());
            }
            return Ok(path);
        }
        units.push(unit);
    }
    Err("unterminated UTF-16 path".into())
}

fn emit(path: &Path, event: &Value) -> Fallible<()> {
    let mut bytes = serde_json::to_vec(event).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(&bytes))
        .map_err(|error| error.to_string())
}

fn read_preset(path: &Path) -> Fallible<Preset> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_JSON + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_JSON {
        return Err("preset exceeds 64 KiB".into());
    }
    let text = std::str::from_utf8(&bytes).map_err(|error| error.to_string())?;
    parse_preset(text).map_err(|error| error.to_string())
}

fn near_delta(call: usize, target: usize) -> Option<i32> {
    i32::try_from(target as i128 - (call as i128 + 5)).ok()
}

fn relay(call: usize, target: usize) -> Fallible<(usize, i32)> {
    const GRANULARITY: usize = 64 * 1024;
    let center = call & !(GRANULARITY - 1);
    for distance in (GRANULARITY..=i32::MAX as usize).step_by(GRANULARITY) {
        for address in [center.checked_add(distance), center.checked_sub(distance)]
            .into_iter()
            .flatten()
        {
            let Some(delta) = near_delta(call, address) else {
                continue;
            };
            if address == 0 {
                continue;
            }
            if !region(address)
                .is_some_and(|info| info.State == windows_sys::Win32::System::Memory::MEM_FREE)
            {
                continue;
            }
            let allocation = unsafe {
                VirtualAlloc(
                    address as *const c_void,
                    14,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            };
            if allocation.is_null() {
                continue;
            }
            if allocation as usize != address {
                unsafe { VirtualFree(allocation, 0, MEM_RELEASE) };
                continue;
            }
            let mut trampoline = [0u8; 14];
            trampoline[..2].copy_from_slice(&[0xff, 0x25]);
            trampoline[6..].copy_from_slice(&(target as u64).to_le_bytes());
            unsafe { ptr::copy_nonoverlapping(trampoline.as_ptr(), allocation.cast(), 14) };
            let mut previous = 0;
            let executable =
                unsafe { VirtualProtect(allocation, 14, PAGE_EXECUTE_READ, &mut previous) };
            if executable == 0
                || unsafe { FlushInstructionCache(GetCurrentProcess(), allocation, 14) } == 0
            {
                unsafe { VirtualFree(allocation, 0, MEM_RELEASE) };
                return Err("could not publish relay executable bytes".into());
            }
            return Ok((address, delta));
        }
    }
    Err("no relay allocation in CALL rel32 range".into())
}

fn replace_call(call: usize, original: [u8; 5], delta: i32) -> Fallible<()> {
    let target = call as *mut c_void;
    let mut previous = 0;
    if unsafe { VirtualProtect(target, 5, PAGE_READWRITE, &mut previous) } == 0 {
        return Err("could not make CALL patch writable".into());
    }
    let mut patch = original;
    patch[1..].copy_from_slice(&delta.to_le_bytes());
    unsafe { ptr::copy_nonoverlapping(patch.as_ptr(), target.cast(), 5) };
    let flushed = unsafe { FlushInstructionCache(GetCurrentProcess(), target, 5) } != 0;
    let mut ignored = 0;
    if flushed && unsafe { VirtualProtect(target, 5, previous, &mut ignored) } != 0 {
        return Ok(());
    }
    unsafe {
        ptr::copy_nonoverlapping(original.as_ptr(), target.cast(), 5);
        FlushInstructionCache(GetCurrentProcess(), target, 5);
        VirtualProtect(target, 5, previous, &mut ignored);
    }
    Err("could not publish CALL patch; original bytes restored".into())
}

fn matching_original_hash(hash: *const u8, hash_len: usize) -> bool {
    hash_len == ORIGINAL_HASH.len()
        && accessible(hash as usize, hash_len, Access::Read)
        && unsafe { read::<[u8; 64]>(hash as usize) } == *ORIGINAL_HASH
}

fn install(
    base: usize,
    image_size: usize,
    hash: *const u8,
    hash_len: usize,
    preset_path: *const u16,
    log: &Path,
) -> Fallible<()> {
    if !matching_original_hash(hash, hash_len) {
        return Err("original image hash mismatch".into());
    }
    let call = image_address(base, image_size, CALL_RVA, ORIGINAL_CALL.len())?;
    let parser = image_address(base, image_size, PARSER_RVA, 1)?;
    let profile = image_address(base, image_size, PROFILE_RVA, 1)?;
    let globals = image_address(base, image_size, TABLE_RVA, 18)?;
    let loaded = image_address(base, image_size, LOADED_RVA, 1)?;
    if !accessible(call, 5, Access::Read)
        || !accessible(call, 5, Access::Execute)
        || !accessible(parser, 1, Access::Execute)
        || !accessible(profile, 1, Access::Execute)
        || !accessible(globals, 18, Access::Read)
        || !accessible(loaded, 1, Access::Read)
    {
        return Err("mapped image pages unavailable".into());
    }
    if unsafe { read::<[u8; 5]>(call) } != ORIGINAL_CALL
        || near_delta(call, parser)
            != Some(i32::from_le_bytes(ORIGINAL_CALL[1..].try_into().unwrap()))
        || unsafe { read::<u8>(loaded) } != 0
    {
        return Err("mapped call identity or startup state mismatch".into());
    }
    let path = wide_path(preset_path)?;
    let preset = read_preset(&path)?;
    let mut state = INSTALLED.lock().map_err(|_| "installer state poisoned")?;
    if state.is_some() {
        return Err("preset already installed".into());
    }
    let (allocation, delta) = relay(call, wrapped_parser as *const () as usize)?;
    if let Err(error) = emit(
        log,
        &json!({"phase":"installing","preset":preset.id,"dry_run":preset.dry_run}),
    ) {
        unsafe { VirtualFree(allocation as *mut c_void, 0, MEM_RELEASE) };
        return Err(error);
    }
    if let Err(error) = replace_call(call, ORIGINAL_CALL, delta) {
        unsafe { VirtualFree(allocation as *mut c_void, 0, MEM_RELEASE) };
        return Err(error);
    }
    *state = Some(Installed {
        base,
        length: image_size,
        preset,
        log: log.to_owned(),
    });
    let _ = emit(
        log,
        &json!({"phase":"installed","call_rva":CALL_RVA,"parser_rva":PARSER_RVA}),
    );
    Ok(())
}

/// Called by the adapter before the guest begins execution. The adapter
/// provides the original-file digest and pins this DLL for the process lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn gp_install_v1(
    image_base: usize,
    image_size: u32,
    original_hash: *const u8,
    hash_len: usize,
    preset_path: *const u16,
    log_path: *const u16,
) -> u32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(log) = wide_path(log_path) else {
            return 1;
        };
        match install(
            image_base,
            image_size as usize,
            original_hash,
            hash_len,
            preset_path,
            &log,
        ) {
            Ok(()) => 0,
            Err(error) => {
                let _ = emit(
                    &log,
                    &json!({"phase":"refused","stage":"install","reason":error}),
                );
                2
            }
        }
    }))
    .unwrap_or(3)
}

fn parser_path(path: *const u8) -> String {
    let mut bytes = Vec::new();
    for index in 0..512 {
        let Some(address) = (path as usize).checked_add(index) else {
            break;
        };
        if !accessible(address, 1, Access::Read) {
            break;
        }
        let byte = unsafe { read::<u8>(address) };
        if byte == 0 {
            return String::from_utf8_lossy(&bytes).into_owned();
        }
        bytes.push(byte);
    }
    "<unreadable or too long>".into()
}

unsafe extern "system" fn wrapped_parser(path: *const u8) -> u8 {
    // The adapter and call-site check establish a live installation before
    // title execution. Recover the lock to ensure the original call always runs.
    let state = INSTALLED.lock().unwrap_or_else(|error| error.into_inner());
    let Some(state) = state.as_ref() else {
        return 0;
    };
    let parser: Parser = unsafe { std::mem::transmute(state.base + PARSER_RVA) };
    let result = unsafe { parser(path) };
    let observed = catch_unwind(AssertUnwindSafe(|| apply(state, path, result)));
    match observed {
        Ok(Ok(())) => {}
        Ok(Err(reason)) => {
            let _ = emit(
                &state.log,
                &json!({"phase":"refused","reason":reason,"parser_al":result}),
            );
        }
        Err(_) => {
            let _ = emit(
                &state.log,
                &json!({"phase":"refused","reason":"preset callback panic","parser_al":result}),
            );
        }
    }
    result
}

fn apply(state: &Installed, path: *const u8, result: u8) -> Fallible<()> {
    let globals = image_address(state.base, state.length, TABLE_RVA, 18)?;
    let loaded = image_address(state.base, state.length, LOADED_RVA, 1)?;
    let profile = image_address(state.base, state.length, PROFILE_RVA, 1)?;
    if !accessible(globals, 18, Access::Read)
        || !accessible(loaded, 1, Access::Read)
        || !accessible(profile, 1, Access::Execute)
        || unsafe { read::<u8>(loaded) } != 0
    {
        return Err("visual table unavailable or already published".into());
    }
    let table = unsafe { read::<usize>(globals) };
    let count = usize::from(unsafe { read::<u16>(globals + 8) });
    let capacity = usize::from(unsafe { read::<u16>(globals + 16) });
    let path = parser_path(path);
    emit(
        &state.log,
        &json!({"phase":"observed","path":path,"parser_al":result,"count":count,
                            "capacity":capacity,"preset":state.preset.id}),
    )?;
    // The platform overlay is optional. The caller has already loaded the base table.
    if count == 0 || count > capacity || table % ROW != 0 {
        return Err("visual table header is invalid".into());
    }
    let bytes = count.checked_mul(ROW).ok_or("row size overflow")?;
    if !accessible(table, bytes, Access::Read) || !accessible(table, bytes, Access::Write) {
        return Err("visual table storage is not readable and writable".into());
    }
    let getter: ProfileGetter = unsafe { std::mem::transmute(profile) };
    let suffix = unsafe { getter() } as usize;
    let image_end = state
        .base
        .checked_add(state.length)
        .ok_or("image end overflow")?;
    if suffix < state.base
        || suffix.checked_add(5).is_none_or(|end| end > image_end)
        || !accessible(suffix, 5, Access::Read)
    {
        return Err("profile getter returned a pointer outside the readable image".into());
    }
    let suffix = unsafe { read::<[u8; 5]>(suffix) };
    let platform = match &suffix {
        b".xbs\0" => Platform::SeriesS,
        b".xbx\0" => Platform::SeriesX,
        _ => return Err("unsupported native profile".into()),
    };
    let rows: Vec<_> = (0..count)
        .map(|index| {
            let address = table + index * ROW;
            RawRow {
                hash: read_bits(address),
                value: f32::from_bits(read_bits(address + 4)),
            }
        })
        .collect();
    let changes = plan(&state.preset, &rows, platform).map_err(|error| error.to_string())?;
    let mut writes = Vec::with_capacity(changes.len());
    for change in &changes {
        if change.index >= count {
            return Err("preset plan referenced a row outside the table".into());
        }
        let address = table + change.index * ROW;
        if read_bits(address) != change.hash || read_bits(address + 4) != change.before.to_bits() {
            return Err("table row changed before preset application".into());
        }
        writes.push((address + 4, change.before.to_bits(), change.after.to_bits()));
    }
    let details: Vec<_> = changes.iter().map(|change| json!({
        "key": change.key, "index": change.index, "hash": change.hash, "selection": change.selection,
        "old": change.before, "new": change.after
    })).collect();
    if state.preset.dry_run {
        return emit(
            &state.log,
            &json!({"phase":"planned","path":path,"parser_al":result,
                                       "count":count,"preset":state.preset.id,"changes":details}),
        );
    }
    // Construct the receipt before mutation. Validate every write and every
    // readback; the synchronous caller has not sorted/published the table yet.
    let applied = json!({"phase":"applied","path":path,"parser_al":result,
                         "count":count,"preset":state.preset.id,"changes":details});
    for &(address, _, after) in &writes {
        write_bits(address, after);
    }
    let readback = writes
        .iter()
        .all(|&(address, _, after)| read_bits(address) == after);
    if readback && emit(&state.log, &applied).is_ok() {
        return Ok(());
    }
    for &(address, before, _) in &writes {
        write_bits(address, before);
    }
    if !writes
        .iter()
        .all(|&(address, before, _)| read_bits(address) == before)
    {
        return Err("preset apply failed; rollback readback failed".into());
    }
    Err("preset apply or receipt failed; old values restored".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "system" fn original(arg: *const u8) -> u8 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { *arg ^ 0x8d }
    }

    unsafe extern "system" fn wrapped(arg: *const u8) -> u8 {
        unsafe { original(arg) }
    }

    struct Allocation(usize);
    impl Drop for Allocation {
        fn drop(&mut self) {
            unsafe { VirtualFree(self.0 as *mut c_void, 0, MEM_RELEASE) };
        }
    }

    #[test]
    fn original_hash_matches_adapter_ascii_abi() {
        let adapter_hash = String::from_utf8(ORIGINAL_HASH.to_vec()).unwrap();
        assert!(matching_original_hash(
            adapter_hash.as_ptr(),
            adapter_hash.len()
        ));
        let mut binary = [0; 32];
        for (index, byte) in binary.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&adapter_hash[index * 2..index * 2 + 2], 16).unwrap();
        }
        assert!(!matching_original_hash(binary.as_ptr(), binary.len()));
        let mut wrong_hash = adapter_hash.into_bytes();
        wrong_hash[0] = b'c';
        assert!(!matching_original_hash(
            wrong_hash.as_ptr(),
            wrong_hash.len()
        ));
        assert!(!matching_original_hash(ptr::null(), 64));
    }

    #[test]
    fn rejects_invalid_page_and_address_ranges() {
        assert!(!accessible(0, 4, Access::Read));
        assert!(!accessible(usize::MAX - 1, 4, Access::Read));
        assert!(image_address(0x1000, 100, 97, 4).is_err());
        assert!(image_address(usize::MAX - 5, 10, 4, 2).is_err());
        assert_eq!(near_delta(0x1000, 0x1005), Some(0));
        assert_eq!(near_delta(0x1000, 0x1004), Some(-1));
    }

    #[test]
    fn relay_preserves_argument_return_and_single_original_call() {
        let code =
            unsafe { VirtualAlloc(ptr::null(), 32, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        assert!(!code.is_null());
        let code_guard = Allocation(code as usize);
        // Win64 caller: allocate shadow space with aligned stack, CALL the
        // relay, restore stack, return the AL supplied by the wrapped callee.
        let authored = [
            0x48u8, 0x83, 0xec, 0x28, 0xe8, 0, 0, 0, 0, 0x48, 0x83, 0xc4, 0x28, 0xc3,
        ];
        unsafe { ptr::copy_nonoverlapping(authored.as_ptr(), code.cast(), authored.len()) };
        let call = code_guard.0 + 4;
        let (thunk, delta) = relay(call, wrapped as *const () as usize).unwrap();
        let thunk_guard = Allocation(thunk);
        let mut old = 0;
        assert_ne!(
            unsafe { VirtualProtect(code, authored.len(), PAGE_EXECUTE_READ, &mut old) },
            0
        );
        replace_call(call, authored[4..9].try_into().unwrap(), delta).unwrap();
        assert!(accessible(thunk_guard.0, 14, Access::Execute));
        assert!(!accessible(thunk_guard.0, 14, Access::Write));
        let invoke: Parser = unsafe { std::mem::transmute(code_guard.0) };
        CALLS.store(0, Ordering::Relaxed);
        let arg = 0x76;
        assert_eq!(unsafe { invoke(&arg) }, arg ^ 0x8d);
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
    }
}
