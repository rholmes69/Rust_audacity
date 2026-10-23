//! VST3 hosting: loading `.vst3` bundles, running their audio processor, parameters,
//! state, and (on Windows) their editor window.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::{CStr, c_char, c_void},
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, Mutex, Weak},
};

use anyhow::{Context, Result, anyhow, bail};
use vst3::{
    Class, ComPtr, ComWrapper, Interface,
    Steinberg::{Vst::*, *},
};

use super::{AudioProcess, ParamInfo, ParamQueue, PluginDesc, PluginFormat, SharedProcess};

// ------------------------------------------------------------------ helpers

fn tuid_to_guid(t: &TUID) -> [u8; 16] {
    let mut g = [0u8; 16];
    for (o, i) in g.iter_mut().zip(t) {
        *o = *i as u8;
    }
    g
}

fn guid_to_tuid(g: &[u8; 16]) -> TUID {
    let mut t: TUID = [0; 16];
    for (o, i) in t.iter_mut().zip(g) {
        *o = *i as c_char;
    }
    t
}

fn hex(g: &[u8; 16]) -> String {
    g.iter().map(|b| format!("{b:02X}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut g = [0u8; 16];
    for (i, b) in g.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(g)
}

fn cstr(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

// TChar is u16 on some platforms and i16 on others.
#[allow(clippy::unnecessary_cast)]
fn wstr(buf: &[TChar]) -> String {
    let units: Vec<u16> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u16)
        .collect();
    String::from_utf16_lossy(&units)
}

#[allow(clippy::unnecessary_cast)]
fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        n += 1;
    }
    if let Some(end) = dst.get_mut(n.min(dst.len().saturating_sub(1))) {
        *end = 0;
    }
}

fn ok(r: tresult) -> bool {
    r == kResultOk
}

// ------------------------------------------------------------------ module loading

/// A loaded `.vst3` library, shared by every plugin instance from that file.
pub struct Module {
    lib: libloading::Library,
    factory: Option<ComPtr<IPluginFactory>>,
}

// The factory is only used from the main thread; the module itself is just kept alive.
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

impl Module {
    fn factory(&self) -> &ComPtr<IPluginFactory> {
        self.factory
            .as_ref()
            .expect("factory is present until drop")
    }
}

impl Drop for Module {
    fn drop(&mut self) {
        // Release the factory before the library's exit function runs.
        drop(self.factory.take());
        unsafe {
            #[cfg(target_os = "linux")]
            if let Ok(exit) = self
                .lib
                .get::<unsafe extern "C" fn() -> bool>(b"ModuleExit")
            {
                exit();
            }
            #[cfg(any(windows, wincheck))]
            if let Ok(exit) = self
                .lib
                .get::<unsafe extern "system" fn() -> bool>(b"ExitDll")
            {
                exit();
            }
        }
    }
}

/// The binary inside a `.vst3` bundle (or the file itself for single-file plugins).
pub fn binary_path(bundle: &Path) -> Result<PathBuf> {
    if bundle.is_file() {
        return Ok(bundle.to_path_buf());
    }
    let stem = bundle
        .file_stem()
        .ok_or_else(|| anyhow!("bad plugin path"))?
        .to_string_lossy()
        .into_owned();
    let contents = bundle.join("Contents");
    let candidates: Vec<PathBuf> = if cfg!(any(windows, wincheck)) {
        vec![contents.join("x86_64-win").join(format!("{stem}.vst3"))]
    } else if cfg!(target_os = "macos") {
        vec![contents.join("MacOS").join(&stem)]
    } else {
        vec![
            contents
                .join(format!("{}-linux", std::env::consts::ARCH))
                .join(format!("{stem}.so")),
            contents.join("x86_64-linux").join(format!("{stem}.so")),
        ]
    };
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("{} has no binary for this computer", bundle.display()))
}

static MODULES: Mutex<Option<HashMap<PathBuf, Weak<Module>>>> = Mutex::new(None);

pub fn load_module(bundle: &Path) -> Result<Arc<Module>> {
    let mut cache = MODULES.lock().unwrap();
    let map = cache.get_or_insert_with(HashMap::new);
    if let Some(m) = map.get(bundle).and_then(Weak::upgrade) {
        return Ok(m);
    }
    if cfg!(target_os = "macos") {
        bail!("VST3 plugins on macOS aren't supported yet");
    }
    let bin = binary_path(bundle)?;
    unsafe {
        let lib =
            libloading::Library::new(&bin).with_context(|| format!("loading {}", bin.display()))?;
        #[cfg(target_os = "linux")]
        {
            if let Ok(entry) = lib.get::<unsafe extern "C" fn(*mut c_void) -> bool>(b"ModuleEntry")
            {
                // The handle is only used by some plugins to find their resources; a fresh
                // dlopen of the same file returns the same handle.
                let h = libloading::os::unix::Library::new(&bin)?.into_raw();
                if !entry(h) {
                    bail!("the plugin's ModuleEntry failed");
                }
            }
        }
        #[cfg(any(windows, wincheck))]
        {
            if let Ok(init) = lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll")
                && !init()
            {
                bail!("the plugin's InitDll failed");
            }
        }
        let get = lib
            .get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory")
            .context("not a VST3 plugin (no GetPluginFactory)")?;
        let factory =
            ComPtr::from_raw(get()).ok_or_else(|| anyhow!("the plugin returned no factory"))?;
        let m = Arc::new(Module {
            lib,
            factory: Some(factory),
        });
        map.insert(bundle.to_path_buf(), Arc::downgrade(&m));
        Ok(m)
    }
}

/// List the audio effects in a `.vst3` bundle.
pub fn scan(bundle: &Path) -> Result<Vec<PluginDesc>> {
    let m = load_module(bundle)?;
    let f = m.factory();
    let f2 = f.cast::<IPluginFactory2>();
    let mut vendor = String::new();
    unsafe {
        let mut fi: PFactoryInfo = std::mem::zeroed();
        if ok(f.getFactoryInfo(&mut fi)) {
            vendor = cstr(&fi.vendor);
        }
    }
    let mut out = Vec::new();
    let n = unsafe { f.countClasses() };
    for i in 0..n {
        unsafe {
            let mut ci: PClassInfo = std::mem::zeroed();
            if !ok(f.getClassInfo(i, &mut ci)) || cstr(&ci.category) != "Audio Module Class" {
                continue;
            }
            let (mut sub, mut v) = (String::new(), vendor.clone());
            if let Some(f2) = &f2 {
                let mut c2: PClassInfo2 = std::mem::zeroed();
                if ok(f2.getClassInfo2(i, &mut c2)) {
                    sub = cstr(&c2.subCategories);
                    let cv = cstr(&c2.vendor);
                    if !cv.is_empty() {
                        v = cv;
                    }
                }
            }
            out.push(PluginDesc {
                format: PluginFormat::Vst3,
                path: bundle.to_path_buf(),
                id: hex(&tuid_to_guid(&ci.cid)),
                name: cstr(&ci.name),
                vendor: v,
                is_instrument: sub.split('|').any(|s| s == "Instrument"),
                category: sub,
            });
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ host objects

/// IHostApplication: the plugin's view of us. It can also create messages for plugins
/// whose processor and controller talk to each other.
struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        unsafe { copy_wstring("Rust Audacity", &mut *name) };
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        unsafe {
            let cid = tuid_to_guid(&*cid);
            let iid = tuid_to_guid(&*iid);
            if cid == IMessage::IID && iid == IMessage::IID {
                let m = ComWrapper::new(HostMessage::default());
                if let Some(p) = m.to_com_ptr::<IMessage>() {
                    *obj = p.into_raw() as *mut c_void;
                    return kResultOk;
                }
            }
            if cid == IAttributeList::IID && iid == IAttributeList::IID {
                let a = ComWrapper::new(AttrList::default());
                if let Some(p) = a.to_com_ptr::<IAttributeList>() {
                    *obj = p.into_raw() as *mut c_void;
                    return kResultOk;
                }
            }
            *obj = ptr::null_mut();
            kResultFalse
        }
    }
}

#[derive(Clone)]
enum Attr {
    Int(i64),
    Float(f64),
    Str(Vec<TChar>),
    Bin(Vec<u8>),
}

#[derive(Default)]
struct AttrList {
    map: RefCell<HashMap<String, Attr>>,
}

impl Class for AttrList {
    type Interfaces = (IAttributeList,);
}

unsafe fn attr_key(id: IAttributeList_::AttrID) -> String {
    unsafe { CStr::from_ptr(id).to_string_lossy().into_owned() }
}

impl IAttributeListTrait for AttrList {
    unsafe fn setInt(&self, id: IAttributeList_::AttrID, value: int64) -> tresult {
        self.map
            .borrow_mut()
            .insert(unsafe { attr_key(id) }, Attr::Int(value));
        kResultOk
    }
    unsafe fn getInt(&self, id: IAttributeList_::AttrID, value: *mut int64) -> tresult {
        match self.map.borrow().get(&unsafe { attr_key(id) }) {
            Some(Attr::Int(v)) => {
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setFloat(&self, id: IAttributeList_::AttrID, value: f64) -> tresult {
        self.map
            .borrow_mut()
            .insert(unsafe { attr_key(id) }, Attr::Float(value));
        kResultOk
    }
    unsafe fn getFloat(&self, id: IAttributeList_::AttrID, value: *mut f64) -> tresult {
        match self.map.borrow().get(&unsafe { attr_key(id) }) {
            Some(Attr::Float(v)) => {
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setString(&self, id: IAttributeList_::AttrID, string: *const TChar) -> tresult {
        let mut v = Vec::new();
        unsafe {
            let mut p = string;
            while *p != 0 {
                v.push(*p);
                p = p.add(1);
            }
        }
        v.push(0);
        self.map
            .borrow_mut()
            .insert(unsafe { attr_key(id) }, Attr::Str(v));
        kResultOk
    }
    unsafe fn getString(
        &self,
        id: IAttributeList_::AttrID,
        string: *mut TChar,
        size_in_bytes: uint32,
    ) -> tresult {
        match self.map.borrow().get(&unsafe { attr_key(id) }) {
            Some(Attr::Str(v)) => {
                let max = size_in_bytes as usize / std::mem::size_of::<TChar>();
                if max == 0 {
                    return kResultFalse;
                }
                let n = v.len().min(max);
                unsafe {
                    ptr::copy_nonoverlapping(v.as_ptr(), string, n);
                    *string.add(n - 1) = 0;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setBinary(
        &self,
        id: IAttributeList_::AttrID,
        data: *const c_void,
        size_in_bytes: uint32,
    ) -> tresult {
        let bytes =
            unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }
                .to_vec();
        self.map
            .borrow_mut()
            .insert(unsafe { attr_key(id) }, Attr::Bin(bytes));
        kResultOk
    }
    unsafe fn getBinary(
        &self,
        id: IAttributeList_::AttrID,
        data: *mut *const c_void,
        size_in_bytes: *mut uint32,
    ) -> tresult {
        // The pointer must stay valid while the list lives, so hand out the stored bytes.
        match self.map.borrow().get(&unsafe { attr_key(id) }) {
            Some(Attr::Bin(v)) => {
                unsafe {
                    *data = v.as_ptr() as *const c_void;
                    *size_in_bytes = v.len() as uint32;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }
}

#[derive(Default)]
struct HostMessage {
    id: RefCell<Vec<u8>>,
    attrs: RefCell<Option<ComWrapper<AttrList>>>,
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        let id = self.id.borrow();
        if id.is_empty() {
            c"".as_ptr()
        } else {
            id.as_ptr() as FIDString
        }
    }
    unsafe fn setMessageID(&self, id: FIDString) {
        let mut v = if id.is_null() {
            Vec::new()
        } else {
            unsafe { CStr::from_ptr(id) }.to_bytes().to_vec()
        };
        v.push(0);
        *self.id.borrow_mut() = v;
    }
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        let mut a = self.attrs.borrow_mut();
        let list = a.get_or_insert_with(|| ComWrapper::new(AttrList::default()));
        // Borrowed reference: the message keeps it alive.
        list.as_com_ref::<IAttributeList>()
            .map_or(ptr::null_mut(), |r| r.as_ptr())
    }
}

/// IBStream over a byte vector (plugin state).
#[derive(Default)]
struct MemStream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl Class for MemStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let n = (num_bytes.max(0) as usize).min(data.len() - pos);
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr().add(pos), buffer as *mut u8, n);
            if !read.is_null() {
                *read = n as int32;
            }
        }
        self.pos.set(pos + n);
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, written: *mut int32) -> tresult {
        let n = num_bytes.max(0) as usize;
        let src = unsafe { std::slice::from_raw_parts(buffer as *const u8, n) };
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + n {
            data.resize(pos + n, 0);
        }
        data[pos..pos + n].copy_from_slice(src);
        self.pos.set(pos + n);
        if !written.is_null() {
            unsafe { *written = n as int32 };
        }
        kResultOk
    }
    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let len = self.data.borrow().len() as i64;
        let base = match mode as IBStream_::IStreamSeekMode {
            IBStream_::IStreamSeekMode_::kIBSeekSet => 0,
            IBStream_::IStreamSeekMode_::kIBSeekCur => self.pos.get() as i64,
            IBStream_::IStreamSeekMode_::kIBSeekEnd => len,
            _ => return kInvalidArgument,
        };
        let p = (base + pos).max(0);
        self.pos.set(p as usize);
        if !result.is_null() {
            unsafe { *result = p };
        }
        kResultOk
    }
    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if !pos.is_null() {
            unsafe { *pos = self.pos.get() as int64 };
        }
        kResultOk
    }
}

fn stream_with(bytes: &[u8]) -> ComWrapper<MemStream> {
    let s = MemStream::default();
    *s.data.borrow_mut() = bytes.to_vec();
    ComWrapper::new(s)
}

/// IComponentHandler: edits made in the plugin's own window are forwarded to the
/// audio processor (and remembered so our parameter list stays in sync).
struct ComponentHandler {
    queue: ParamQueue,
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        self.queue.push(id, value);
        kResultOk
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn restartComponent(&self, _flags: int32) -> tresult {
        kResultOk
    }
}

/// One parameter's changes for one block (we only ever send a single point).
struct ValueQueue {
    id: ParamID,
    value: Cell<ParamValue>,
}

impl Class for ValueQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ValueQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id
    }
    unsafe fn getPointCount(&self) -> int32 {
        1
    }
    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index != 0 {
            return kResultFalse;
        }
        unsafe {
            *offset = 0;
            *value = self.value.get();
        }
        kResultOk
    }
    unsafe fn addPoint(&self, _offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        self.value.set(value);
        if !index.is_null() {
            unsafe { *index = 0 };
        }
        kResultOk
    }
}

#[derive(Default)]
struct ParamChanges {
    queues: RefCell<Vec<ComWrapper<ValueQueue>>>,
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.queues.borrow().len() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        self.queues
            .borrow()
            .get(index as usize)
            .and_then(|q| q.as_com_ref::<IParamValueQueue>())
            .map_or(ptr::null_mut(), |r| r.as_ptr())
    }
    unsafe fn addParameterData(
        &self,
        id: *const ParamID,
        index: *mut int32,
    ) -> *mut IParamValueQueue {
        let id = unsafe { *id };
        let mut qs = self.queues.borrow_mut();
        let k = match qs.iter().position(|q| q.id == id) {
            Some(k) => k,
            None => {
                qs.push(ComWrapper::new(ValueQueue {
                    id,
                    value: Cell::new(0.0),
                }));
                qs.len() - 1
            }
        };
        if !index.is_null() {
            unsafe { *index = k as int32 };
        }
        qs[k]
            .as_com_ref::<IParamValueQueue>()
            .map_or(ptr::null_mut(), |r| r.as_ptr())
    }
}

// ------------------------------------------------------------------ audio side

/// Runs on the audio thread.
struct Processor {
    processor: ComPtr<IAudioProcessor>,
    queue: ParamQueue,
    changes: ComWrapper<ParamChanges>,
    /// Changes the plugin reports back (ignored, but some plugins require the object).
    out_changes: ComWrapper<ParamChanges>,
    in_ch: usize,
    out_ch: usize,
    max_block: usize,
    rate: f64,
    ins: Vec<Vec<f32>>,
    outs: Vec<Vec<f32>>,
    pending: Vec<(u32, f64)>,
}

// The VST3 audio processor may be called from any one thread at a time; the mutex
// around `Processor` guarantees that.
unsafe impl Send for Processor {}

impl Processor {
    fn run(&mut self, frames: usize) {
        // Parameter changes since the last block (from our UI or the plugin's window).
        self.pending.clear();
        self.queue.drain_into(&mut self.pending);
        {
            let mut qs = self.changes.queues.borrow_mut();
            qs.clear();
            for &(id, v) in &self.pending {
                qs.push(ComWrapper::new(ValueQueue {
                    id,
                    value: Cell::new(v),
                }));
            }
        }
        let mut in_ptrs: Vec<*mut f32> = self.ins.iter_mut().map(|b| b.as_mut_ptr()).collect();
        let mut out_ptrs: Vec<*mut f32> = self.outs.iter_mut().map(|b| b.as_mut_ptr()).collect();
        let mut in_bus = AudioBusBuffers {
            numChannels: self.in_ch as int32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: in_ptrs.as_mut_ptr(),
            },
        };
        let mut out_bus = AudioBusBuffers {
            numChannels: self.out_ch as int32,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 {
                channelBuffers32: out_ptrs.as_mut_ptr(),
            },
        };
        self.out_changes.queues.borrow_mut().clear();
        let out_changes = self
            .out_changes
            .as_com_ref::<IParameterChanges>()
            .map_or(ptr::null_mut(), |r| r.as_ptr());
        let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
        ctx.sampleRate = self.rate;
        let changes = if self.pending.is_empty() {
            ptr::null_mut()
        } else {
            self.changes
                .as_com_ref::<IParameterChanges>()
                .map_or(ptr::null_mut(), |r| r.as_ptr())
        };
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            numSamples: frames as int32,
            numInputs: if self.in_ch > 0 { 1 } else { 0 },
            numOutputs: 1,
            inputs: if self.in_ch > 0 {
                &mut in_bus
            } else {
                ptr::null_mut()
            },
            outputs: &mut out_bus,
            inputParameterChanges: changes,
            outputParameterChanges: out_changes,
            inputEvents: ptr::null_mut(),
            outputEvents: ptr::null_mut(),
            processContext: &mut ctx,
        };
        unsafe {
            self.processor.process(&mut data);
        }
    }
}

impl AudioProcess for Processor {
    fn process(&mut self, chans: &mut [Vec<f32>]) {
        let len = chans.first().map_or(0, Vec::len);
        let mut off = 0;
        while off < len {
            let n = (len - off).min(self.max_block);
            for (c, buf) in self.ins.iter_mut().enumerate() {
                let src = &chans[c.min(chans.len() - 1)];
                buf[..n].copy_from_slice(&src[off..off + n]);
            }
            for b in self.outs.iter_mut() {
                b[..n].fill(0.0);
            }
            self.run(n);
            write_back(chans, &self.outs, off, n);
            off += n;
        }
    }

    fn flush_params(&mut self) {
        if !self.queue.is_empty() {
            self.run(0);
        }
    }

    fn set_rate(&mut self, rate: f64) {
        self.rate = rate;
    }
}

/// Copy plugin output into the track's channels (stereo plugins on mono tracks are mixed
/// down; mono plugins on stereo tracks feed both channels).
pub(super) fn write_back(chans: &mut [Vec<f32>], outs: &[Vec<f32>], off: usize, n: usize) {
    if outs.is_empty() {
        return;
    }
    if chans.len() == 1 && outs.len() >= 2 {
        for i in 0..n {
            chans[0][off + i] = (outs[0][i] + outs[1][i]) * 0.5;
        }
    } else {
        for (c, ch) in chans.iter_mut().enumerate() {
            let o = &outs[c.min(outs.len() - 1)];
            ch[off..off + n].copy_from_slice(&o[..n]);
        }
    }
}

// ------------------------------------------------------------------ main-thread side

pub struct Vst3Plugin {
    desc: PluginDesc,
    _module: Arc<Module>,
    _host: ComWrapper<HostApp>,
    _handler: ComWrapper<ComponentHandler>,
    component: ComPtr<IComponent>,
    processor: Option<ComPtr<IAudioProcessor>>,
    controller: Option<ComPtr<IEditController>>,
    /// Connection points when processor and controller are separate objects.
    connected: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    controller_is_component: bool,
    shared: SharedProcess,
    queue: ParamQueue,
    rate: f64,
    max_block: usize,
    #[cfg(any(windows, wincheck))]
    editor: Option<super::win_editor::Editor>,
}

impl Vst3Plugin {
    pub fn load(desc: &PluginDesc, rate: f64, max_block: usize) -> Result<Self> {
        let module = load_module(&desc.path)?;
        let cid = guid_to_tuid(&unhex(&desc.id).ok_or_else(|| anyhow!("bad VST3 class id"))?);
        let host = ComWrapper::new(HostApp);
        let host_unk = host
            .as_com_ref::<IHostApplication>()
            .map(|r| r.as_ptr() as *mut FUnknown)
            .ok_or_else(|| anyhow!("host object"))?;
        let queue = ParamQueue::default();
        let handler = ComWrapper::new(ComponentHandler {
            queue: queue.clone(),
        });
        unsafe {
            if let Some(f3) = module.factory().cast::<IPluginFactory3>() {
                f3.setHostContext(host_unk);
            }
            let mut obj: *mut c_void = ptr::null_mut();
            let r = module.factory().createInstance(
                cid.as_ptr(),
                IComponent::IID.as_ptr() as *const c_char,
                &mut obj,
            );
            if !ok(r) || obj.is_null() {
                bail!("{} could not be created", desc.name);
            }
            let component =
                ComPtr::from_raw(obj as *mut IComponent).ok_or_else(|| anyhow!("no component"))?;
            if !ok(component.initialize(host_unk)) {
                bail!("{} failed to initialize", desc.name);
            }
            let processor = component
                .cast::<IAudioProcessor>()
                .ok_or_else(|| anyhow!("{} has no audio processor", desc.name))?;

            // Controller: the same object, or a separate class.
            let (controller, controller_is_component) = match component.cast::<IEditController>() {
                Some(c) => (Some(c), true),
                None => {
                    let mut ccid: TUID = [0; 16];
                    let mut ctl = None;
                    if ok(component.getControllerClassId(&mut ccid)) && ccid != [0; 16] {
                        let mut o: *mut c_void = ptr::null_mut();
                        if ok(module.factory().createInstance(
                            ccid.as_ptr(),
                            IEditController::IID.as_ptr() as *const c_char,
                            &mut o,
                        )) && let Some(c) = ComPtr::from_raw(o as *mut IEditController)
                            && ok(c.initialize(host_unk))
                        {
                            ctl = Some(c);
                        }
                    }
                    (ctl, false)
                }
            };
            let mut connected = None;
            if let Some(c) = &controller {
                if let Some(h) = handler.as_com_ref::<IComponentHandler>() {
                    c.setComponentHandler(h.as_ptr());
                }
                if !controller_is_component
                    && let (Some(a), Some(b)) = (
                        component.cast::<IConnectionPoint>(),
                        c.cast::<IConnectionPoint>(),
                    )
                {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                    connected = Some((a, b));
                }
                // Let the controller know the processor's state.
                let s = ComWrapper::new(MemStream::default());
                if let Some(sp) = s.as_com_ref::<IBStream>()
                    && ok(component.getState(sp.as_ptr()))
                {
                    s.pos.set(0);
                    c.setComponentState(sp.as_ptr());
                }
            }

            // Buses: one main input and output, stereo if the plugin allows it.
            let n_in = component.getBusCount(
                MediaTypes_::kAudio as MediaType,
                BusDirections_::kInput as BusDirection,
            );
            let n_out = component.getBusCount(
                MediaTypes_::kAudio as MediaType,
                BusDirections_::kOutput as BusDirection,
            );
            if n_out < 1 {
                bail!("{} has no audio output", desc.name);
            }
            let mut ins_arr = vec![SpeakerArr::kStereo; n_in.max(0) as usize];
            let mut outs_arr = vec![SpeakerArr::kStereo; n_out as usize];
            if !ok(processor.setBusArrangements(
                ins_arr.as_mut_ptr(),
                n_in,
                outs_arr.as_mut_ptr(),
                n_out,
            )) {
                // Keep whatever the plugin prefers.
            }
            let bus_channels = |dir: BusDirections| -> usize {
                let mut bi: BusInfo = std::mem::zeroed();
                if ok(component.getBusInfo(
                    MediaTypes_::kAudio as MediaType,
                    dir as BusDirection,
                    0,
                    &mut bi,
                )) {
                    bi.channelCount.max(0) as usize
                } else {
                    0
                }
            };
            let in_ch = if n_in > 0 {
                bus_channels(BusDirections_::kInput)
            } else {
                0
            };
            let out_ch = bus_channels(BusDirections_::kOutput).max(1);
            if n_in > 0 {
                component.activateBus(
                    MediaTypes_::kAudio as MediaType,
                    BusDirections_::kInput as BusDirection,
                    0,
                    1,
                );
            }
            component.activateBus(
                MediaTypes_::kAudio as MediaType,
                BusDirections_::kOutput as BusDirection,
                0,
                1,
            );
            if processor.canProcessSampleSize(SymbolicSampleSizes_::kSample32 as int32) != kResultOk
            {
                bail!("{} doesn't support 32-bit float processing", desc.name);
            }

            let proc = Processor {
                processor: processor.clone(),
                queue: queue.clone(),
                changes: ComWrapper::new(ParamChanges::default()),
                out_changes: ComWrapper::new(ParamChanges::default()),
                in_ch,
                out_ch,
                max_block,
                rate,
                ins: vec![vec![0.0; max_block]; in_ch],
                outs: vec![vec![0.0; max_block]; out_ch],
                pending: Vec::with_capacity(64),
            };
            let shared: SharedProcess = Arc::new(Mutex::new(Box::new(proc)));
            let mut p = Self {
                desc: desc.clone(),
                _module: module,
                _host: host,
                _handler: handler,
                component,
                processor: Some(processor),
                controller,
                connected,
                controller_is_component,
                shared,
                queue,
                rate,
                max_block,
                #[cfg(any(windows, wincheck))]
                editor: None,
            };
            p.activate()?;
            Ok(p)
        }
    }

    fn activate(&mut self) -> Result<()> {
        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as int32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
            maxSamplesPerBlock: self.max_block as int32,
            sampleRate: self.rate,
        };
        unsafe {
            let Some(processor) = &self.processor else {
                bail!("plugin unloaded")
            };
            if !ok(processor.setupProcessing(&mut setup)) {
                bail!(
                    "{} rejected the sample rate {} Hz",
                    self.desc.name,
                    self.rate
                );
            }
            self.component.setActive(1);
            processor.setProcessing(1);
        }
        if let Ok(mut g) = self.shared.lock() {
            g.set_rate(self.rate);
        }
        Ok(())
    }

    fn deactivate(&mut self) {
        let _g = self.shared.lock();
        unsafe {
            if let Some(p) = &self.processor {
                p.setProcessing(0);
            }
            self.component.setActive(0);
        }
    }

    pub fn desc(&self) -> &PluginDesc {
        &self.desc
    }

    pub fn processor(&self) -> SharedProcess {
        self.shared.clone()
    }

    pub fn set_rate(&mut self, rate: f64) -> Result<()> {
        if (rate - self.rate).abs() < 0.5 {
            return Ok(());
        }
        self.deactivate();
        self.rate = rate;
        self.activate()
    }

    pub fn params(&self) -> Vec<ParamInfo> {
        let Some(c) = &self.controller else {
            return Vec::new();
        };
        let mut out = Vec::new();
        unsafe {
            for i in 0..c.getParameterCount() {
                let mut pi: ParameterInfo = std::mem::zeroed();
                if !ok(c.getParameterInfo(i, &mut pi)) {
                    continue;
                }
                use ParameterInfo_::ParameterFlags_::*;
                let flags = pi.flags as u32;
                if flags & (kIsHidden | kIsReadOnly) as u32 != 0 {
                    continue;
                }
                out.push(ParamInfo {
                    id: pi.id,
                    name: wstr(&pi.title),
                    unit: wstr(&pi.units),
                    min: 0.0,
                    max: 1.0,
                    default: pi.defaultNormalizedValue,
                    value: c.getParamNormalized(pi.id),
                    steps: pi.stepCount.max(0) as u32,
                    is_bypass: flags & kIsBypass as u32 != 0,
                });
            }
        }
        out
    }

    pub fn param_text(&self, id: u32, value: f64) -> Option<String> {
        let c = self.controller.as_ref()?;
        unsafe {
            let mut s: String128 = [0; 128];
            ok(c.getParamStringByValue(id, value, &mut s)).then(|| wstr(&s))
        }
    }

    pub fn set_param(&mut self, id: u32, value: f64) {
        let v = value.clamp(0.0, 1.0);
        if let Some(c) = &self.controller {
            unsafe {
                c.setParamNormalized(id, v);
            }
        }
        self.queue.push(id, v);
    }

    /// Deliver queued parameter changes when nothing is playing, so the processor (and
    /// therefore its saved state) is up to date.
    pub fn idle(&mut self) {
        if !self.queue.is_empty()
            && let Ok(mut g) = self.shared.try_lock()
        {
            g.flush_params();
        }
        #[cfg(any(windows, wincheck))]
        if let Some(e) = &self.editor {
            e.poll();
            if e.closed() {
                self.close_editor();
            }
        }
    }

    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        self.idle();
        let read = |f: &dyn Fn(*mut IBStream) -> tresult| -> Vec<u8> {
            let s = ComWrapper::new(MemStream::default());
            if let Some(sp) = s.as_com_ref::<IBStream>()
                && ok(f(sp.as_ptr()))
            {
                return s.data.borrow().clone();
            }
            Vec::new()
        };
        let comp = {
            let _g = self.shared.lock();
            read(&|p| unsafe { self.component.getState(p) })
        };
        let ctl = match (&self.controller, self.controller_is_component) {
            (Some(c), false) => read(&|p| unsafe { c.getState(p) }),
            _ => Vec::new(),
        };
        let mut out = b"RAV3".to_vec();
        out.extend((comp.len() as u32).to_le_bytes());
        out.extend(&comp);
        out.extend((ctl.len() as u32).to_le_bytes());
        out.extend(&ctl);
        // The controller's parameter values too: some controllers don't pick their values
        // up from the processor state, so they are restored explicitly on load.
        let mut vals: Vec<(u32, f64)> = Vec::new();
        if let Some(c) = &self.controller {
            unsafe {
                for i in 0..c.getParameterCount() {
                    let mut pi: ParameterInfo = std::mem::zeroed();
                    if ok(c.getParameterInfo(i, &mut pi)) {
                        vals.push((pi.id, c.getParamNormalized(pi.id)));
                    }
                }
            }
        }
        out.extend((vals.len() as u32).to_le_bytes());
        for (id, v) in vals {
            out.extend(id.to_le_bytes());
            out.extend(v.to_le_bytes());
        }
        Ok(out)
    }

    pub fn load_state(&mut self, data: &[u8]) -> Result<()> {
        let bad = || anyhow!("this plugin state is damaged");
        if data.len() < 12 || &data[..4] != b"RAV3" {
            return Err(bad());
        }
        let n1 = u32::from_le_bytes(data[4..8].try_into()?) as usize;
        let comp = data.get(8..8 + n1).ok_or_else(bad)?;
        let rest = &data[8 + n1..];
        let n2 = u32::from_le_bytes(rest.get(..4).ok_or_else(bad)?.try_into()?) as usize;
        let ctl = rest.get(4..4 + n2).ok_or_else(bad)?;
        let mut vals: Vec<(u32, f64)> = Vec::new();
        if let Some(tail) = rest.get(4 + n2..)
            && tail.len() >= 4
        {
            let n = u32::from_le_bytes(tail[..4].try_into()?) as usize;
            for k in 0..n {
                let o = 4 + k * 12;
                let Some(b) = tail.get(o..o + 12) else { break };
                vals.push((
                    u32::from_le_bytes(b[..4].try_into()?),
                    f64::from_le_bytes(b[4..].try_into()?),
                ));
            }
        }
        unsafe {
            if !comp.is_empty() {
                let s = stream_with(comp);
                let sp = s.as_com_ref::<IBStream>().ok_or_else(bad)?;
                {
                    let _g = self.shared.lock();
                    self.component.setState(sp.as_ptr());
                }
                if let Some(c) = &self.controller {
                    s.pos.set(0);
                    c.setComponentState(sp.as_ptr());
                }
            }
            if !ctl.is_empty()
                && let Some(c) = &self.controller
                && !self.controller_is_component
            {
                let s = stream_with(ctl);
                if let Some(sp) = s.as_com_ref::<IBStream>() {
                    c.setState(sp.as_ptr());
                }
            }
            if let Some(c) = &self.controller {
                for (id, v) in vals {
                    if (c.getParamNormalized(id) - v).abs() > 1e-9 {
                        c.setParamNormalized(id, v);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn has_editor(&self) -> bool {
        cfg!(any(windows, wincheck)) && self.controller.is_some()
    }

    #[cfg(any(windows, wincheck))]
    pub fn open_editor(&mut self, title: &str) -> Result<()> {
        if self.editor.is_some() {
            return Ok(());
        }
        let c = self
            .controller
            .as_ref()
            .ok_or_else(|| anyhow!("no editor"))?;
        let view = unsafe { c.createView(c"editor".as_ptr()) };
        let view = unsafe { ComPtr::from_raw(view) }
            .ok_or_else(|| anyhow!("{} has no editor window", self.desc.name))?;
        self.editor = Some(super::win_editor::Editor::open_vst3(view, title)?);
        Ok(())
    }

    #[cfg(not(any(windows, wincheck)))]
    pub fn open_editor(&mut self, _title: &str) -> Result<()> {
        bail!("plugin windows are only available on Windows for now; use the sliders")
    }

    pub fn close_editor(&mut self) {
        #[cfg(any(windows, wincheck))]
        if let Some(e) = self.editor.take() {
            e.close();
        }
    }

    pub fn editor_open(&self) -> bool {
        #[cfg(any(windows, wincheck))]
        {
            self.editor.is_some()
        }
        #[cfg(not(any(windows, wincheck)))]
        {
            false
        }
    }
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        self.close_editor();
        self.deactivate();
        // Release the audio side's reference before the component is terminated (the
        // shared handle may outlive us on the audio thread; it becomes a pass-through).
        if let Ok(mut g) = self.shared.lock() {
            *g = Box::new(super::Bypass);
        }
        drop(self.processor.take());
        unsafe {
            if let Some((a, b)) = self.connected.take() {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(c) = self.controller.take() {
                c.setComponentHandler(ptr::null_mut());
                if !self.controller_is_component {
                    c.terminate();
                }
            }
            self.component.terminate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let g = [1u8, 2, 3, 0xAB, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0xFF];
        assert_eq!(unhex(&hex(&g)), Some(g));
    }
}
