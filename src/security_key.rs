//! Offline native FIDO2 diagnostics, not network login or WebAuthn forwarding.
//! The private FFI uses the public libfido2 C ABI; no CTAP/crypto implementation.
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use ring::digest;
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    ptr,
};
use zeroize::Zeroizing;

const ES256: c_int = -7;
const OPT_FALSE: c_int = 1;
const OPT_TRUE: c_int = 2;
const MAX_DEVICES: usize = 32;

#[derive(Subcommand)]
pub enum Command {
    /// List local FIDO devices without creating credentials or requesting a PIN.
    List,
    /// Read a local key's FIDO2/PIN/UV capabilities (no credential creation).
    Probe {
        #[arg(long)]
        device: Option<String>,
    },
    /// Create an OFFLINE test credential on a key; does not enable Teleport login.
    Enroll {
        /// Trusted host SHA-256 certificate fingerprint (64 hexadecimal characters).
        #[arg(long)]
        fingerprint: String,
        /// New private file for the public credential; never overwrites a file.
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        device: Option<String>,
    },
    /// Request and verify an offline assertion; does not connect to a host.
    Verify {
        #[arg(long)]
        credential: PathBuf,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        device: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    version: u32,
    fingerprint: String,
    rp: String,
    id: Vec<u8>,
    public_key: Vec<u8>,
    algorithm: i32,
    uv_required: bool,
}

fn fingerprint(value: &str) -> Result<String> {
    ensure!(
        value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "fingerprint must be exactly 64 hexadecimal characters copied from a trusted host"
    );
    Ok(value.to_ascii_lowercase())
}
fn relying_party(value: &str) -> Result<String> {
    let value = fingerprint(value)?;
    // Two 32-character labels preserve all 256 bits without exceeding DNS limits.
    // This native CTAP namespace is deliberately not a WebAuthn website origin.
    Ok(format!(
        "f-{}.{}.teleport.invalid",
        &value[..32],
        &value[32..]
    ))
}
fn challenge_hash(
    fingerprint: &str,
    rp: &str,
    purpose: &str,
    challenge: [u8; 32],
) -> Result<Vec<u8>> {
    let encoded = serde_json::to_vec(&(
        "teleport/native-fido2/offline/v1",
        fingerprint,
        rp,
        purpose,
        challenge,
    ))?;
    Ok(digest::digest(&digest::SHA256, &encoded).as_ref().to_vec())
}
fn validate_credential(value: &Credential, expected: &str) -> Result<()> {
    let expected = fingerprint(expected)?;
    ensure!(
        value.version == 1 && value.algorithm == ES256 && value.uv_required,
        "unsupported credential version, algorithm or verification policy"
    );
    ensure!(
        value.fingerprint == expected && value.rp == relying_party(&expected)?,
        "credential belongs to a different host identity"
    );
    ensure!(
        !value.id.is_empty() && value.id.len() <= 4096 && value.public_key.len() == 64,
        "invalid credential ID or ES256 key length"
    );
    Ok(())
}

fn validate_single_allowed_id(returned: &[u8], expected: &[u8]) -> Result<()> {
    // CTAP may omit the descriptor when exactly one allow-list entry was sent.
    // This is safe only together with verification against that entry's key.
    ensure!(
        returned.is_empty() || returned == expected,
        "authenticator returned a different credential"
    );
    Ok(())
}

// Signatures checked against upstream fido.h/fido/es256.h. All native handles
// remain private and are freed before their owning Api library is unloaded.
macro_rules! native_api {
    ($($name:ident: fn($($arg:ty),*) -> $ret:ty),+ $(,)?) => {
        struct Api { $($name: unsafe extern "C" fn($($arg),*) -> $ret,)+ _library: libloading::Library }
        impl Api {
            fn load() -> Result<Self> {
                let name = std::env::var_os("TELEPORT_LIBFIDO2").map(PathBuf::from).context("libfido2 not configured; run the packaged app or `nix develop` (TELEPORT_LIBFIDO2 must name a trusted absolute library path)")?;
                ensure!(name.is_absolute(), "TELEPORT_LIBFIDO2 must be an absolute path to a trusted library");
                // SAFETY: the explicitly selected native library is trusted executable
                // code. Symbol signatures match its documented stable public C ABI.
                unsafe {
                    let library = libloading::Library::new(name).context("load libfido2")?;
                    $(let $name = *library.get::<unsafe extern "C" fn($($arg),*) -> $ret>(concat!(stringify!($name), "\0").as_bytes()).context(concat!("missing libfido2 symbol ", stringify!($name)))?;)+
                    let api = Self { $($name,)+ _library: library };
                    (api.fido_init)(2); // FIDO_DISABLE_U2F_FALLBACK; no debug logging.
                    Ok(api)
                }
            }
        }
    };
}
native_api! {
    fido_init: fn(c_int) -> (),
    fido_strerr: fn(c_int) -> *const c_char,
    fido_dev_info_new: fn(usize) -> *mut c_void,
    fido_dev_info_free: fn(*mut *mut c_void, usize) -> (),
    fido_dev_info_manifest: fn(*mut c_void, usize, *mut usize) -> c_int,
    fido_dev_info_ptr: fn(*const c_void, usize) -> *const c_void,
    fido_dev_info_path: fn(*const c_void) -> *const c_char,
    fido_dev_info_manufacturer_string: fn(*const c_void) -> *const c_char,
    fido_dev_info_product_string: fn(*const c_void) -> *const c_char,
    fido_dev_new: fn() -> *mut c_void,
    fido_dev_free: fn(*mut *mut c_void) -> (),
    fido_dev_open: fn(*mut c_void, *const c_char) -> c_int,
    fido_dev_close: fn(*mut c_void) -> c_int,
    fido_dev_cancel: fn(*mut c_void) -> c_int,
    fido_dev_set_timeout: fn(*mut c_void, c_int) -> c_int,
    fido_dev_is_fido2: fn(*const c_void) -> bool,
    fido_dev_has_pin: fn(*const c_void) -> bool,
    fido_dev_has_uv: fn(*const c_void) -> bool,
    fido_dev_supports_pin: fn(*const c_void) -> bool,
    fido_dev_supports_uv: fn(*const c_void) -> bool,
    fido_cred_new: fn() -> *mut c_void,
    fido_cred_free: fn(*mut *mut c_void) -> (),
    fido_cred_set_type: fn(*mut c_void, c_int) -> c_int,
    fido_cred_set_clientdata_hash: fn(*mut c_void, *const u8, usize) -> c_int,
    fido_cred_set_rp: fn(*mut c_void, *const c_char, *const c_char) -> c_int,
    fido_cred_set_user: fn(*mut c_void, *const u8, usize, *const c_char, *const c_char, *const c_char) -> c_int,
    fido_cred_set_rk: fn(*mut c_void, c_int) -> c_int,
    fido_cred_set_uv: fn(*mut c_void, c_int) -> c_int,
    fido_dev_make_cred: fn(*mut c_void, *mut c_void, *const c_char) -> c_int,
    fido_cred_verify: fn(*const c_void) -> c_int,
    fido_cred_verify_self: fn(*const c_void) -> c_int,
    fido_cred_x5c_len: fn(*const c_void) -> usize,
    fido_cred_flags: fn(*const c_void) -> u8,
    fido_cred_id_ptr: fn(*const c_void) -> *const u8,
    fido_cred_id_len: fn(*const c_void) -> usize,
    fido_cred_pubkey_ptr: fn(*const c_void) -> *const u8,
    fido_cred_pubkey_len: fn(*const c_void) -> usize,
    fido_assert_new: fn() -> *mut c_void,
    fido_assert_free: fn(*mut *mut c_void) -> (),
    fido_assert_set_rp: fn(*mut c_void, *const c_char) -> c_int,
    fido_assert_set_clientdata_hash: fn(*mut c_void, *const u8, usize) -> c_int,
    fido_assert_set_up: fn(*mut c_void, c_int) -> c_int,
    fido_assert_set_uv: fn(*mut c_void, c_int) -> c_int,
    fido_assert_allow_cred: fn(*mut c_void, *const u8, usize) -> c_int,
    fido_dev_get_assert: fn(*mut c_void, *mut c_void, *const c_char) -> c_int,
    fido_assert_count: fn(*const c_void) -> usize,
    fido_assert_id_ptr: fn(*const c_void, usize) -> *const u8,
    fido_assert_id_len: fn(*const c_void, usize) -> usize,
    fido_assert_verify: fn(*const c_void, usize, c_int, *const c_void) -> c_int,
    fido_assert_sigcount: fn(*const c_void, usize) -> u32,
    fido_assert_set_count: fn(*mut c_void, usize) -> c_int,
    fido_assert_set_authdata_raw: fn(*mut c_void, usize, *const u8, usize) -> c_int,
    fido_assert_set_sig: fn(*mut c_void, usize, *const u8, usize) -> c_int,
    fido_assert_authdata_raw_ptr: fn(*const c_void, usize) -> *const u8,
    fido_assert_authdata_raw_len: fn(*const c_void, usize) -> usize,
    fido_assert_sig_ptr: fn(*const c_void, usize) -> *const u8,
    fido_assert_sig_len: fn(*const c_void, usize) -> usize,
    es256_pk_new: fn() -> *mut c_void,
    es256_pk_free: fn(*mut *mut c_void) -> (),
    es256_pk_from_ptr: fn(*mut c_void, *const c_void, usize) -> c_int,
}

struct Handle<'a> {
    api: &'a Api,
    pointer: *mut c_void,
    free: unsafe extern "C" fn(*mut *mut c_void),
    device_open: bool,
}
impl<'a> Handle<'a> {
    fn new(
        api: &'a Api,
        pointer: *mut c_void,
        free: unsafe extern "C" fn(*mut *mut c_void),
    ) -> Result<Self> {
        ensure!(!pointer.is_null(), "libfido2 allocation failed");
        Ok(Self {
            api,
            pointer,
            free,
            device_open: false,
        })
    }
}
impl Drop for Handle<'_> {
    fn drop(&mut self) {
        // SAFETY: each pointer is owned by this handle and matches its free
        // function; the borrowed library remains alive for the entire drop.
        unsafe {
            if self.device_open {
                (self.api.fido_dev_cancel)(self.pointer);
                (self.api.fido_dev_close)(self.pointer);
            }
            (self.free)(&mut self.pointer);
        }
    }
}
struct DeviceList<'a> {
    api: &'a Api,
    pointer: *mut c_void,
}
impl Drop for DeviceList<'_> {
    fn drop(&mut self) {
        // SAFETY: this allocation was made with MAX_DEVICES elements.
        unsafe {
            (self.api.fido_dev_info_free)(&mut self.pointer, MAX_DEVICES);
        }
    }
}
struct Device {
    path: String,
    manufacturer: String,
    product: String,
}

impl Api {
    fn check(&self, result: c_int) -> Result<()> {
        if result == 0 {
            return Ok(());
        }
        // SAFETY: libfido2 returns a static NUL-terminated error description.
        let error = unsafe { text((self.fido_strerr)(result)) };
        bail!("security-key operation failed: {error} ({result}); no automatic PIN retry")
    }
    fn devices(&self) -> Result<Vec<Device>> {
        // SAFETY: list allocation and bounds follow the public manifest API;
        // strings are copied before the RAII list frees their owner.
        unsafe {
            let pointer = (self.fido_dev_info_new)(MAX_DEVICES);
            ensure!(!pointer.is_null(), "device list allocation failed");
            let list = DeviceList { api: self, pointer };
            let mut count = 0;
            self.check((self.fido_dev_info_manifest)(
                list.pointer,
                MAX_DEVICES,
                &mut count,
            ))?;
            ensure!(count <= MAX_DEVICES, "invalid device count");
            let mut devices = Vec::new();
            for index in 0..count {
                let info = (self.fido_dev_info_ptr)(list.pointer, index);
                ensure!(!info.is_null(), "invalid device descriptor");
                devices.push(Device {
                    path: text((self.fido_dev_info_path)(info)),
                    manufacturer: text((self.fido_dev_info_manufacturer_string)(info)),
                    product: text((self.fido_dev_info_product_string)(info)),
                });
            }
            Ok(devices)
        }
    }
    fn open(&self, selection: Option<&str>) -> Result<Handle<'_>> {
        let devices = self.devices()?;
        let selected = match selection {
            Some(path) => devices
                .iter()
                .find(|d| d.path == path)
                .context("requested key is not in the local device list")?,
            None if devices.len() == 1 => &devices[0],
            None if devices.is_empty() => bail!(
                "no accessible FIDO device found; check connection and local device permissions"
            ),
            None => bail!("multiple keys found; select one with --device from `security-key list`"),
        };
        let path = CString::new(selected.path.as_bytes())?;
        // SAFETY: handle lifetime owns the device; the path remains valid during
        // open. Timeouts are set before opening and before interactive requests.
        unsafe {
            let mut device = Handle::new(self, (self.fido_dev_new)(), self.fido_dev_free)?;
            self.check((self.fido_dev_set_timeout)(device.pointer, 5_000))?;
            self.check((self.fido_dev_open)(device.pointer, path.as_ptr()))?;
            device.device_open = true;
            Ok(device)
        }
    }
    fn pin(&self, device: &Handle<'_>) -> Result<Option<Zeroizing<Vec<u8>>>> {
        // SAFETY: getters read a valid opened device owned by the caller.
        unsafe {
            ensure!(
                (self.fido_dev_is_fido2)(device.pointer),
                "FIDO2 required; U2F fallback is disabled"
            );
            if (self.fido_dev_has_pin)(device.pointer) {
                let pin = Zeroizing::new(rpassword::prompt_password(
                    "Security-key PIN (local only; one attempt): ",
                )?);
                ensure!(
                    !pin.is_empty() && pin.len() <= 255 && !pin.as_bytes().contains(&0),
                    "invalid PIN length or encoding"
                );
                let mut bytes = Zeroizing::new(pin.as_bytes().to_vec());
                bytes.push(0);
                Ok(Some(bytes))
            } else {
                ensure!(
                    (self.fido_dev_has_uv)(device.pointer),
                    "key requires a configured PIN or built-in user verification; configure it with the vendor's trusted tool first"
                );
                Ok(None)
            }
        }
    }
    fn assertion(
        &self,
        device: &Handle<'_>,
        credential: &Credential,
        pin: Option<&[u8]>,
        hash: &[u8],
    ) -> Result<u32> {
        let rp = CString::new(credential.rp.as_bytes())?;
        // SAFETY: all native objects live until their corresponding library calls
        // finish. Input pointers reference bounded Rust buffers for each call.
        unsafe {
            let assertion = Handle::new(self, (self.fido_assert_new)(), self.fido_assert_free)?;
            self.check((self.fido_assert_set_rp)(assertion.pointer, rp.as_ptr()))?;
            self.check((self.fido_assert_set_clientdata_hash)(
                assertion.pointer,
                hash.as_ptr(),
                hash.len(),
            ))?;
            self.check((self.fido_assert_set_up)(assertion.pointer, OPT_TRUE))?;
            self.check((self.fido_assert_set_uv)(assertion.pointer, OPT_TRUE))?;
            self.check((self.fido_assert_allow_cred)(
                assertion.pointer,
                credential.id.as_ptr(),
                credential.id.len(),
            ))?;
            self.check((self.fido_dev_set_timeout)(device.pointer, 30_000))?;
            eprintln!("Touch/verify your security key for this offline host-bound assertion.");
            self.check((self.fido_dev_get_assert)(
                device.pointer,
                assertion.pointer,
                pin.map_or(ptr::null(), |p| p.as_ptr().cast()),
            ))?;
            ensure!(
                (self.fido_assert_count)(assertion.pointer) == 1,
                "expected exactly one assertion"
            );
            let id_length = (self.fido_assert_id_len)(assertion.pointer, 0);
            let id = if id_length == 0 {
                Vec::new()
            } else {
                blob(
                    (self.fido_assert_id_ptr)(assertion.pointer, 0),
                    id_length,
                    4096,
                )?
            };
            validate_single_allowed_id(&id, &credential.id)?;
            let authdata = blob(
                (self.fido_assert_authdata_raw_ptr)(assertion.pointer, 0),
                (self.fido_assert_authdata_raw_len)(assertion.pointer, 0),
                8192,
            )?;
            let signature = blob(
                (self.fido_assert_sig_ptr)(assertion.pointer, 0),
                (self.fido_assert_sig_len)(assertion.pointer, 0),
                1024,
            )?;
            self.verify_assertion(credential, hash, &authdata, &signature)?;
            Ok((self.fido_assert_sigcount)(assertion.pointer, 0))
        }
    }
    fn verify_assertion(
        &self,
        credential: &Credential,
        hash: &[u8],
        authdata: &[u8],
        signature: &[u8],
    ) -> Result<()> {
        ensure!(
            hash.len() == 32
                && (37..=8192).contains(&authdata.len())
                && !signature.is_empty()
                && signature.len() <= 1024
                && credential.public_key.len() == 64,
            "invalid assertion dimensions"
        );
        let rp = CString::new(credential.rp.as_bytes())?;
        // SAFETY: reconstruct a separate verification object from untrusted
        // assertion bytes, with expected RP/hash/UP/UV from local policy.
        unsafe {
            let assertion = Handle::new(self, (self.fido_assert_new)(), self.fido_assert_free)?;
            self.check((self.fido_assert_set_rp)(assertion.pointer, rp.as_ptr()))?;
            self.check((self.fido_assert_set_clientdata_hash)(
                assertion.pointer,
                hash.as_ptr(),
                hash.len(),
            ))?;
            self.check((self.fido_assert_set_up)(assertion.pointer, OPT_TRUE))?;
            self.check((self.fido_assert_set_uv)(assertion.pointer, OPT_TRUE))?;
            self.check((self.fido_assert_set_count)(assertion.pointer, 1))?;
            self.check((self.fido_assert_set_authdata_raw)(
                assertion.pointer,
                0,
                authdata.as_ptr(),
                authdata.len(),
            ))?;
            self.check((self.fido_assert_set_sig)(
                assertion.pointer,
                0,
                signature.as_ptr(),
                signature.len(),
            ))?;
            let public_key = Handle::new(self, (self.es256_pk_new)(), self.es256_pk_free)?;
            self.check((self.es256_pk_from_ptr)(
                public_key.pointer,
                credential.public_key.as_ptr().cast(),
                credential.public_key.len(),
            ))?;
            self.check((self.fido_assert_verify)(
                assertion.pointer,
                0,
                ES256,
                public_key.pointer,
            ))
        }
    }
    fn enroll(
        &self,
        device: &Handle<'_>,
        fingerprint: &str,
        pin: Option<&[u8]>,
    ) -> Result<Credential> {
        let rp_string = relying_party(fingerprint)?;
        let rp = CString::new(rp_string.as_bytes())?;
        let hash = challenge_hash(fingerprint, &rp_string, "create", rand::random())?;
        let user: [u8; 32] = rand::random();
        // SAFETY: owned handles and bounded inputs follow fido_cred_* API.
        unsafe {
            let credential = Handle::new(self, (self.fido_cred_new)(), self.fido_cred_free)?;
            self.check((self.fido_cred_set_type)(credential.pointer, ES256))?;
            self.check((self.fido_cred_set_clientdata_hash)(
                credential.pointer,
                hash.as_ptr(),
                hash.len(),
            ))?;
            self.check((self.fido_cred_set_rp)(
                credential.pointer,
                rp.as_ptr(),
                c"Teleport offline diagnostic".as_ptr(),
            ))?;
            self.check((self.fido_cred_set_user)(
                credential.pointer,
                user.as_ptr(),
                user.len(),
                c"teleport-test".as_ptr(),
                c"Teleport offline diagnostic".as_ptr(),
                ptr::null(),
            ))?;
            self.check((self.fido_cred_set_rk)(credential.pointer, OPT_FALSE))?;
            self.check((self.fido_cred_set_uv)(credential.pointer, OPT_TRUE))?;
            self.check((self.fido_dev_set_timeout)(device.pointer, 30_000))?;
            eprintln!(
                "Creating an offline, non-discoverable credential. Touch/verify your key. This does not enable Teleport login."
            );
            self.check((self.fido_dev_make_cred)(
                device.pointer,
                credential.pointer,
                pin.map_or(ptr::null(), |p| p.as_ptr().cast()),
            ))?;
            ensure!(
                (self.fido_cred_flags)(credential.pointer) & 5 == 5,
                "credential must attest user presence and verification"
            );
            if (self.fido_cred_x5c_len)(credential.pointer) == 0 {
                self.check((self.fido_cred_verify_self)(credential.pointer))?;
            } else {
                self.check((self.fido_cred_verify)(credential.pointer))?;
            }
            let value = Credential {
                version: 1,
                fingerprint: fingerprint.into(),
                rp: rp_string,
                id: blob(
                    (self.fido_cred_id_ptr)(credential.pointer),
                    (self.fido_cred_id_len)(credential.pointer),
                    4096,
                )?,
                public_key: blob(
                    (self.fido_cred_pubkey_ptr)(credential.pointer),
                    (self.fido_cred_pubkey_len)(credential.pointer),
                    64,
                )?,
                algorithm: ES256,
                uv_required: true,
            };
            validate_credential(&value, fingerprint)?;
            // Prove possession before saving, independently of attestation format.
            let hash = challenge_hash(fingerprint, &value.rp, "get", rand::random())?;
            self.assertion(device, &value, pin, &hash)?;
            Ok(value)
        }
    }
}

// SAFETY: callers supply strings owned by live libfido2 objects; null means an
// absent optional descriptor. Debug escaping at display prevents terminal codes.
unsafe fn text(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}
// SAFETY: callers pass pointer/length pairs from the same live native object.
unsafe fn blob(value: *const u8, length: usize, maximum: usize) -> Result<Vec<u8>> {
    ensure!(
        !value.is_null() && length > 0 && length <= maximum,
        "invalid native credential data"
    );
    Ok(unsafe { std::slice::from_raw_parts(value, length) }.to_vec())
}

fn read_credential(path: &Path, expected: &str) -> Result<Credential> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= 32_768,
        "credential must be a bounded regular file, not a symlink"
    );
    let mut bytes = Vec::new();
    file.take(32_769).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 32_768,
        "credential file grew beyond its size limit"
    );
    let credential: Credential = serde_json::from_slice(&bytes)?;
    validate_credential(&credential, expected)?;
    Ok(credential)
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::List => {
            let api = Api::load()?;
            let devices = api.devices()?;
            if devices.is_empty() {
                println!("No accessible FIDO devices found. No hardware operation was requested.");
            }
            for device in devices {
                println!(
                    "{:?}  {:?} {:?}",
                    device.path, device.manufacturer, device.product
                );
            }
        }
        Command::Probe { device } => {
            let api = Api::load()?;
            let device = api.open(device.as_deref())?;
            // SAFETY: getters only inspect the live opened device capability state.
            unsafe {
                println!(
                    "FIDO2: {}\nPIN supported: {}\nPIN configured: {}\nBuilt-in UV supported: {}\nBuilt-in UV configured: {}\nRead-only probe; no credentials created.",
                    (api.fido_dev_is_fido2)(device.pointer),
                    (api.fido_dev_supports_pin)(device.pointer),
                    (api.fido_dev_has_pin)(device.pointer),
                    (api.fido_dev_supports_uv)(device.pointer),
                    (api.fido_dev_has_uv)(device.pointer)
                );
            }
        }
        Command::Enroll {
            fingerprint: value,
            output,
            device,
        } => {
            let value = fingerprint(&value)?;
            let api = Api::load()?;
            let device = api.open(device.as_deref())?;
            // Reserve output before requesting a credential, never truncate user
            // data. On failure retain this empty file as a diagnostic artifact;
            // no automatic key deletion/reset can recover the authenticator state.
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&output)
                .context("create new credential output (will not overwrite)")?;
            let pin = api.pin(&device)?;
            let credential = api.enroll(&device, &value, pin.as_ref().map(|p| p.as_slice())).context("offline enrollment failed; output may be empty and the authenticator may already have created a credential")?;
            file.write_all(&serde_json::to_vec_pretty(&credential)?)?;
            file.sync_all()?;
            println!(
                "Offline credential verified and saved to {}. Not installed as a host login credential; no manufacturer trust-chain policy was evaluated.",
                output.display()
            );
        }
        Command::Verify {
            credential,
            fingerprint: value,
            device,
        } => {
            let value = fingerprint(&value)?;
            let credential = read_credential(&credential, &value)?;
            let api = Api::load()?;
            let device = api.open(device.as_deref())?;
            let pin = api.pin(&device)?;
            let hash = challenge_hash(&value, &credential.rp, "get", rand::random())?;
            let counter = api.assertion(
                &device,
                &credential,
                pin.as_ref().map(|p| p.as_slice()),
                &hash,
            )?;
            println!(
                "Offline assertion verified (presence + user verification), counter={counter}. Fresh random challenge; not a network login or persistent counter/cloning assessment."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn single_allow_list_may_omit_id_but_not_substitute_one() {
        assert!(validate_single_allowed_id(&[], &[1, 2]).is_ok());
        assert!(validate_single_allowed_id(&[1, 2], &[1, 2]).is_ok());
        assert!(validate_single_allowed_id(&[1, 3], &[1, 2]).is_err());
    }
    #[test]
    fn credential_reader_rejects_large_files_and_symlinks() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join("credential.json");
        fs::write(&path, vec![0; 32_769])?;
        assert!(read_credential(&path, &"a".repeat(64)).is_err());
        #[cfg(unix)]
        {
            let link = temporary.path().join("link.json");
            std::os::unix::fs::symlink(&path, &link)?;
            assert!(read_credential(&link, &"a".repeat(64)).is_err());
        }
        Ok(())
    }
    #[test]
    fn fingerprint_rp_and_challenge_binding() -> Result<()> {
        let fingerprint = "A".repeat(64);
        let rp = relying_party(&fingerprint)?;
        assert!(rp.split('.').all(|label| label.len() <= 63));
        assert_eq!(rp, relying_party(&fingerprint.to_lowercase())?);
        for bad in ["", "123", &"g".repeat(64), &"a".repeat(65)] {
            assert!(relying_party(bad).is_err());
        }
        let original = challenge_hash(&fingerprint, &rp, "create", [1; 32])?;
        assert_ne!(original, challenge_hash(&fingerprint, &rp, "get", [1; 32])?);
        assert_ne!(
            original,
            challenge_hash(&fingerprint, &rp, "create", [2; 32])?
        );
        assert_ne!(
            original,
            challenge_hash(&"b".repeat(64), &rp, "create", [1; 32])?
        );
        Ok(())
    }
    #[test]
    fn credential_policy_rejects_downgrade_and_wrong_host() -> Result<()> {
        let expected = "a".repeat(64);
        let mut credential = Credential {
            version: 1,
            fingerprint: expected.clone(),
            rp: relying_party(&expected)?,
            id: vec![1],
            public_key: vec![0; 64],
            algorithm: ES256,
            uv_required: true,
        };
        validate_credential(&credential, &expected)?;
        assert!(validate_credential(&credential, &"b".repeat(64)).is_err());
        credential.uv_required = false;
        assert!(validate_credential(&credential, &expected).is_err());
        credential.uv_required = true;
        credential.algorithm = -257;
        assert!(validate_credential(&credential, &expected).is_err());
        credential.algorithm = ES256;
        credential.id = vec![0; 4097];
        assert!(validate_credential(&credential, &expected).is_err());
        Ok(())
    }
    #[test]
    #[ignore = "requires packaged libfido2 via TELEPORT_LIBFIDO2; no hardware needed"]
    fn native_verifier_rejects_bad_signature_rp_hash_and_uv() -> Result<()> {
        use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
        let api = Api::load()?;
        let rng = ring::rand::SystemRandom::new();
        let document = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, document.as_ref(), &rng)
                .unwrap();
        let fingerprint = "a".repeat(64);
        let mut credential = Credential {
            version: 1,
            fingerprint: fingerprint.clone(),
            rp: relying_party(&fingerprint)?,
            id: vec![1],
            public_key: key.public_key().as_ref()[1..].to_vec(),
            algorithm: ES256,
            uv_required: true,
        };
        let hash = challenge_hash(&fingerprint, &credential.rp, "get", [7; 32])?;
        let mut authdata = digest::digest(&digest::SHA256, credential.rp.as_bytes())
            .as_ref()
            .to_vec();
        authdata.push(5);
        authdata.extend_from_slice(&1u32.to_be_bytes());
        let sign = |authdata: &[u8]| {
            let mut message = authdata.to_vec();
            message.extend_from_slice(&hash);
            key.sign(&rng, &message).unwrap().as_ref().to_vec()
        };
        let signature = sign(&authdata);
        api.verify_assertion(&credential, &hash, &authdata, &signature)?;
        let mut wrong = signature.clone();
        wrong[0] ^= 1;
        assert!(
            api.verify_assertion(&credential, &hash, &authdata, &wrong)
                .is_err()
        );
        assert!(
            api.verify_assertion(&credential, &[0; 32], &authdata, &signature)
                .is_err()
        );
        credential.rp = relying_party(&"b".repeat(64))?;
        assert!(
            api.verify_assertion(&credential, &hash, &authdata, &signature)
                .is_err()
        );
        credential.rp = relying_party(&fingerprint)?;
        authdata[32] = 1;
        assert!(
            api.verify_assertion(&credential, &hash, &authdata, &sign(&authdata))
                .is_err()
        );
        authdata[32] = 4;
        assert!(
            api.verify_assertion(&credential, &hash, &authdata, &sign(&authdata))
                .is_err()
        );
        assert!(
            api.verify_assertion(&credential, &hash, &[], &signature)
                .is_err()
        );
        Ok(())
    }
}
