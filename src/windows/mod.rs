//! The implementation of the [SharedLibrary
//! trait](../trait.SharedLibrary.html) for windows.

use super::Segment as SegmentTrait;
use super::SharedLibrary as SharedLibraryTrait;
use super::{Bias, IterationControl, SharedLibraryId, Svma};

use winapi::ctypes::c_char;
use winapi::shared::guiddef::GUID;
use winapi::shared::minwindef::{HMODULE, MAX_PATH};
use winapi::um::libloaderapi::{FreeLibrary, LoadLibraryExW, LOAD_LIBRARY_AS_DATAFILE};
use winapi::um::memoryapi::VirtualQuery;
use winapi::um::processthreadsapi::GetCurrentProcess;
use winapi::um::psapi::{
    EnumProcessModules, GetModuleFileNameExW, GetModuleInformation, MODULEINFO,
};
use winapi::um::winnt::{
    IMAGE_DEBUG_DIRECTORY, IMAGE_DEBUG_TYPE_CODEVIEW, IMAGE_DIRECTORY_ENTRY_DEBUG,
    IMAGE_DOS_HEADER, IMAGE_DOS_SIGNATURE, IMAGE_NT_HEADERS, IMAGE_NT_SIGNATURE,
    IMAGE_SECTION_HEADER, MEMORY_BASIC_INFORMATION, MEM_COMMIT,
};

const CV_SIGNATURE: u32 = 0x5344_5352;

use std::ffi::{CStr, OsStr, OsString};
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::os::windows::ffi::OsStringExt;
use std::ptr;
use std::slice;
use std::usize;

/// An unsupported segment
pub struct Segment<'a> {
    section: &'a IMAGE_SECTION_HEADER,
    phantom: PhantomData<&'a SharedLibrary<'a>>,
}

impl<'a> fmt::Debug for Segment<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Segment")
            .field("name", &self.name())
            .field("is_code", &self.is_code())
            .finish()
    }
}

impl<'a> SegmentTrait for Segment<'a> {
    type SharedLibrary = ::windows::SharedLibrary<'a>;

    #[inline]
    fn name(&self) -> &OsStr {
        let cstr = unsafe { CStr::from_ptr(self.section.Name.as_ptr() as *const i8) };
        if let Ok(s) = cstr.to_str() {
            OsStr::new(s)
        } else {
            OsStr::new("")
        }
    }

    fn is_code(&self) -> bool {
        self.name() == OsStr::new(".text")
    }

    #[inline]
    fn stated_virtual_memory_address(&self) -> Svma {
        Svma(self.section.VirtualAddress as *const u8)
    }

    #[inline]
    fn len(&self) -> usize {
        self.section.SizeOfRawData as usize
    }
}

/// An iterator over Mach-O segments.
pub struct SegmentIter<'a> {
    sections: &'a [IMAGE_SECTION_HEADER],
    phantom: PhantomData<&'a SharedLibrary<'a>>,
}

impl<'a> fmt::Debug for SegmentIter<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SegmentIter").finish()
    }
}

impl<'a> Iterator for SegmentIter<'a> {
    type Item = Segment<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.sections.is_empty() {
            None
        } else {
            let section = &self.sections[0];
            self.sections = &self.sections[1..];
            Some(Segment {
                section,
                phantom: PhantomData,
            })
        }
    }
}

#[repr(C)]
struct CodeViewRecord70 {
    signature: u32,
    pdb_signature: GUID,
    pdb_age: u32,
    pdb_filename: [u8; 1],
}

/// The fallback implementation of the [SharedLibrary
/// trait](../trait.SharedLibrary.html).
pub struct SharedLibrary<'a> {
    module_info: MODULEINFO,
    module_name: OsString,
    phantom: PhantomData<&'a SharedLibrary<'a>>,
}

impl<'a> fmt::Debug for SharedLibrary<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("SharedLibrary")
            .field("module_base", &self.module_base())
            .field("name", &self.name())
            .field("debug_name", &self.debug_name())
            .field("id", &self.id())
            .field("debug_id", &self.debug_id())
            .finish()
    }
}

impl<'a> SharedLibrary<'a> {
    fn new(module_info: MODULEINFO, module_name: OsString) -> SharedLibrary<'a> {
        SharedLibrary {
            module_info,
            module_name,
            phantom: PhantomData,
        }
    }

    #[inline]
    fn module_base(&self) -> *const c_char {
        self.module_info.lpBaseOfDll as *const c_char
    }

    fn dos_header(&self) -> Option<&IMAGE_DOS_HEADER> {
        let header: &IMAGE_DOS_HEADER = unsafe { mem::transmute(self.module_base()) };
        if header.e_magic == IMAGE_DOS_SIGNATURE {
            Some(header)
        } else {
            None
        }
    }

    fn nt_headers(&self) -> Option<&IMAGE_NT_HEADERS> {
        self.dos_header().and_then(|dos_header| {
            let nt_headers: &IMAGE_NT_HEADERS =
                unsafe { mem::transmute(self.module_base().offset(dos_header.e_lfanew as isize)) };
            if nt_headers.Signature == IMAGE_NT_SIGNATURE {
                Some(nt_headers)
            } else {
                println!("NOT FOUND {:x}", nt_headers.Signature);
                None
            }
        })
    }

    fn codeview_record70(&self) -> Option<&CodeViewRecord70> {
        let bias = self.virtual_memory_bias().0;
        unsafe {
            let debug_dictionary: *const IMAGE_DEBUG_DIRECTORY =
                mem::transmute(self.module_base().offset(bias));
            if debug_dictionary.is_null() || (*debug_dictionary).Type != IMAGE_DEBUG_TYPE_CODEVIEW {
                return None;
            }
            let debug_info: *const CodeViewRecord70 = mem::transmute(
                self.module_base()
                    .offset((*debug_dictionary).AddressOfRawData as isize),
            );
            if debug_info.is_null() || (*debug_info).signature != CV_SIGNATURE {
                return None;
            }
            Some(&*debug_info)
        }
    }
}

impl<'a> SharedLibraryTrait for SharedLibrary<'a> {
    type Segment = Segment<'a>;
    type SegmentIter = SegmentIter<'a>;

    #[inline]
    fn name(&self) -> &OsStr {
        &self.module_name
    }

    #[inline]
    fn debug_name(&self) -> Option<&OsStr> {
        self.codeview_record70().and_then(|codeview| unsafe {
            let bytes: *const i8 = mem::transmute(&codeview.pdb_filename);
            let cstr = CStr::from_ptr(bytes);
            if let Ok(s) = cstr.to_str() {
                Some(OsStr::new(s))
            } else {
                None
            }
        })
    }

    fn id(&self) -> Option<SharedLibraryId> {
        self.nt_headers().map(|nt_headers| {
            SharedLibraryId::PeSignature(
                nt_headers.FileHeader.TimeDateStamp,
                nt_headers.OptionalHeader.SizeOfImage,
            )
        })
    }

    #[inline]
    fn debug_id(&self) -> Option<SharedLibraryId> {
        self.codeview_record70().map(|codeview| unsafe {
            SharedLibraryId::PdbSignature(mem::transmute(codeview.pdb_signature), codeview.pdb_age)
        })
    }

    fn segments(&self) -> Self::SegmentIter {
        let sections = self.nt_headers().map(|nt_headers| unsafe {
            let base =
                (nt_headers as *const _ as *const u8).add(mem::size_of::<IMAGE_NT_HEADERS>());
            slice::from_raw_parts(
                base as *const IMAGE_SECTION_HEADER,
                nt_headers.FileHeader.NumberOfSections as usize,
            )
        });
        SegmentIter {
            sections: sections.unwrap_or(&[][..]),
            phantom: PhantomData,
        }
    }

    #[inline]
    fn virtual_memory_bias(&self) -> Bias {
        Bias(self.nt_headers().map_or(0, |nt_headers| {
            nt_headers.OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_DEBUG as usize]
                .VirtualAddress as isize
        }))
    }

    fn each<F, C>(mut f: F)
    where
        F: FnMut(&Self) -> C,
        C: Into<IterationControl>,
    {
        let proc = unsafe { GetCurrentProcess() };
        let mut modules_size = 0;
        unsafe {
            if EnumProcessModules(proc, ptr::null_mut(), 0, &mut modules_size) == 0 {
                return;
            }
        }
        let module_count = modules_size / mem::size_of::<HMODULE>() as u32;
        let mut modules = vec![unsafe { mem::zeroed() }; module_count as usize];
        unsafe {
            if EnumProcessModules(proc, modules.as_mut_ptr(), modules_size, &mut modules_size) == 0
            {
                return;
            }
        }

        modules.truncate(modules_size as usize / mem::size_of::<HMODULE>());

        for module in modules.iter_mut() {
            unsafe {
                let mut module_path = vec![0u16; MAX_PATH + 1];
                if GetModuleFileNameExW(
                    proc,
                    *module,
                    module_path.as_mut_ptr(),
                    MAX_PATH as u32 + 1,
                ) == 0
                {
                    continue;
                }

                let mut module_info = mem::zeroed();
                if !GetModuleInformation(
                    proc,
                    *module,
                    &mut module_info,
                    mem::size_of::<MODULEINFO>() as u32,
                ) == 0
                {
                    continue;
                }

                // to prevent something else from unloading the module while
                // we're poking around in memory we load it a second time.  This
                // will effectively just increment the refcount since it has been
                // loaded before.
                let handle_lock = LoadLibraryExW(
                    module_path.as_ptr(),
                    ptr::null_mut(),
                    LOAD_LIBRARY_AS_DATAFILE,
                );

                let mut vmem_info = mem::zeroed();
                let mut should_break = false;
                if VirtualQuery(
                    module_info.lpBaseOfDll,
                    &mut vmem_info,
                    mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                ) == mem::size_of::<MEMORY_BASIC_INFORMATION>()
                {
                    let module_path = OsString::from_wide(
                        &module_path[..module_path.iter().position(|x| *x == 0).unwrap_or(0)],
                    );
                    if vmem_info.State == MEM_COMMIT {
                        let shlib = SharedLibrary::new(module_info, module_path);
                        match f(&shlib).into() {
                            IterationControl::Break => should_break = true,
                            IterationControl::Continue => {}
                        }
                    }
                }

                FreeLibrary(handle_lock);

                if should_break {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{IterationControl, Segment, SharedLibrary};
    use windows;

    #[test]
    fn can_break() {
        let mut first_count = 0;
        windows::SharedLibrary::each(|_| {
            first_count += 1;
        });
        assert!(first_count > 2);

        let mut second_count = 0;
        windows::SharedLibrary::each(|_| {
            second_count += 1;

            if second_count == first_count - 1 {
                IterationControl::Break
            } else {
                IterationControl::Continue
            }
        });
        assert_eq!(second_count, first_count - 1);
    }

    #[test]
    fn get_name() {
        windows::SharedLibrary::each(|shlib| {
            let _ = shlib.name();
            assert!(shlib.debug_name().is_some());
        });
    }

    #[test]
    fn have_code() {
        windows::SharedLibrary::each(|shlib| {
            println!("shlib = {:?}", shlib.name());

            let mut found_code = false;
            for seg in shlib.segments() {
                println!("    segment = {:?}", seg.name());
                if seg.is_code() {
                    found_code = true;
                }
            }
            assert!(found_code);
        });
    }

    #[test]
    fn get_id() {
        windows::SharedLibrary::each(|shlib| {
            assert!(shlib.id().is_some());
            assert!(shlib.debug_id().is_some());
        });
    }
}
