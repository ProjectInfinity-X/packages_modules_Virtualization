/*
 * Copyright (C) 2021 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use anyhow::{anyhow, bail, Context, Result};
use libc::getxattr;
use log::error;
use minijail::{self, Minijail};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;

use authfs_aidl_interface::aidl::com::android::virt::fs::{
    AuthFsConfig::AuthFsConfig, IAuthFs::IAuthFs, IAuthFsService::IAuthFsService,
    InputFdAnnotation::InputFdAnnotation, OutputFdAnnotation::OutputFdAnnotation,
};
use authfs_aidl_interface::binder::{ParcelFileDescriptor, Strong};
use compos_aidl_interface::aidl::com::android::compos::Metadata::Metadata;

/// The number that represents the file descriptor number expecting by the task. The number may be
/// meaningless in the current process.
pub type PseudoRawFd = i32;

const SHA256_HASH_SIZE: usize = 32;
type Sha256Hash = [u8; SHA256_HASH_SIZE];

pub enum CompilerOutput {
    /// Fs-verity digests of output files, if the compiler finishes successfully.
    Digests { oat: Sha256Hash, vdex: Sha256Hash, image: Sha256Hash },
    /// Exit code returned by the compiler, if not 0.
    ExitCode(i8),
}

struct CompilerOutputParcelFds {
    oat: ParcelFileDescriptor,
    vdex: ParcelFileDescriptor,
    image: ParcelFileDescriptor,
}

/// Runs the compiler with given flags with file descriptors described in `metadata` retrieved via
/// `authfs_service`. Returns exit code of the compiler process.
pub fn compile(
    compiler_path: &Path,
    compiler_args: &[String],
    authfs_service: Strong<dyn IAuthFsService>,
    metadata: &Metadata,
) -> Result<CompilerOutput> {
    // Mount authfs (via authfs_service). The authfs instance unmounts once the `authfs` variable
    // is out of scope.
    let authfs_config = build_authfs_config(metadata);
    let authfs = authfs_service.mount(&authfs_config)?;

    // The task expects to receive FD numbers that match its flags (e.g. --zip-fd=42) prepared
    // on the host side. Since the local FD opened from authfs (e.g. /authfs/42) may not match
    // the task's expectation, prepare a FD mapping and let minijail prepare the correct FD
    // setup.
    let fd_mapping =
        open_authfs_files_for_fd_mapping(&authfs, &authfs_config).context("Open on authfs")?;

    let jail =
        spawn_jailed_task(compiler_path, compiler_args, fd_mapping).context("Spawn dex2oat")?;
    let jail_result = jail.wait();

    let parcel_fds = parse_compiler_args(&authfs, compiler_args)?;
    let oat_file: &File = parcel_fds.oat.as_ref();
    let vdex_file: &File = parcel_fds.vdex.as_ref();
    let image_file: &File = parcel_fds.image.as_ref();

    match jail_result {
        Ok(()) => Ok(CompilerOutput::Digests {
            oat: fsverity_measure(oat_file.as_raw_fd())?,
            vdex: fsverity_measure(vdex_file.as_raw_fd())?,
            image: fsverity_measure(image_file.as_raw_fd())?,
        }),
        Err(minijail::Error::ReturnCode(exit_code)) => {
            error!("dex2oat failed with exit code {}", exit_code);
            Ok(CompilerOutput::ExitCode(exit_code as i8))
        }
        Err(e) => {
            bail!("Unexpected minijail error: {}", e)
        }
    }
}

fn parse_compiler_args(
    authfs: &Strong<dyn IAuthFs>,
    args: &[String],
) -> Result<CompilerOutputParcelFds> {
    const OAT_FD_PREFIX: &str = "--oat-fd=";
    const VDEX_FD_PREFIX: &str = "--output-vdex-fd=";
    const IMAGE_FD_PREFIX: &str = "--image-fd=";
    const APP_IMAGE_FD_PREFIX: &str = "--app-image-fd=";

    let mut oat = None;
    let mut vdex = None;
    let mut image = None;

    for arg in args {
        if let Some(value) = arg.strip_prefix(OAT_FD_PREFIX) {
            let fd = value.parse::<RawFd>().context("Invalid --oat-fd flag")?;
            debug_assert!(oat.is_none());
            oat = Some(authfs.openFile(fd, false)?);
        } else if let Some(value) = arg.strip_prefix(VDEX_FD_PREFIX) {
            let fd = value.parse::<RawFd>().context("Invalid --output-vdex-fd flag")?;
            debug_assert!(vdex.is_none());
            vdex = Some(authfs.openFile(fd, false)?);
        } else if let Some(value) = arg.strip_prefix(IMAGE_FD_PREFIX) {
            let fd = value.parse::<RawFd>().context("Invalid --image-fd flag")?;
            debug_assert!(image.is_none());
            image = Some(authfs.openFile(fd, false)?);
        } else if let Some(value) = arg.strip_prefix(APP_IMAGE_FD_PREFIX) {
            let fd = value.parse::<RawFd>().context("Invalid --app-image-fd flag")?;
            debug_assert!(image.is_none());
            image = Some(authfs.openFile(fd, false)?);
        }
    }

    Ok(CompilerOutputParcelFds {
        oat: oat.ok_or_else(|| anyhow!("Missing --oat-fd"))?,
        vdex: vdex.ok_or_else(|| anyhow!("Missing --vdex-fd"))?,
        image: image.ok_or_else(|| anyhow!("Missing --image-fd or --app-image-fd"))?,
    })
}

fn build_authfs_config(metadata: &Metadata) -> AuthFsConfig {
    AuthFsConfig {
        port: 3264, // TODO: support dynamic port
        inputFdAnnotations: metadata
            .input_fd_annotations
            .iter()
            .map(|x| InputFdAnnotation { fd: x.fd, fileSize: x.file_size })
            .collect(),
        outputFdAnnotations: metadata
            .output_fd_annotations
            .iter()
            .map(|x| OutputFdAnnotation { fd: x.fd })
            .collect(),
    }
}

fn open_authfs_files_for_fd_mapping(
    authfs: &Strong<dyn IAuthFs>,
    config: &AuthFsConfig,
) -> Result<Vec<(ParcelFileDescriptor, PseudoRawFd)>> {
    let mut fd_mapping = Vec::new();

    let results: Result<Vec<_>> = config
        .inputFdAnnotations
        .iter()
        .map(|annotation| Ok((authfs.openFile(annotation.fd, false)?, annotation.fd)))
        .collect();
    fd_mapping.append(&mut results?);

    let results: Result<Vec<_>> = config
        .outputFdAnnotations
        .iter()
        .map(|annotation| Ok((authfs.openFile(annotation.fd, true)?, annotation.fd)))
        .collect();
    fd_mapping.append(&mut results?);

    Ok(fd_mapping)
}

fn spawn_jailed_task(
    executable: &Path,
    args: &[String],
    fd_mapping: Vec<(ParcelFileDescriptor, PseudoRawFd)>,
) -> Result<Minijail> {
    // TODO(b/185175567): Run in a more restricted sandbox.
    let jail = Minijail::new()?;
    let preserve_fds: Vec<_> = fd_mapping.iter().map(|(f, id)| (f.as_raw_fd(), *id)).collect();
    let _pid = jail.run_remap(executable, preserve_fds.as_slice(), args)?;
    Ok(jail)
}

fn fsverity_measure(fd: RawFd) -> Result<Sha256Hash> {
    // TODO(b/196635431): Unfortunately, the FUSE API doesn't allow authfs to implement the standard
    // fs-verity ioctls. Until the kernel allows, use the alternative xattr that authfs provides.
    let path = CString::new(format!("/proc/self/fd/{}", fd).as_str()).unwrap();
    let name = CString::new("authfs.fsverity.digest").unwrap();
    let mut buf = [0u8; SHA256_HASH_SIZE];
    // SAFETY: getxattr should not write beyond the given buffer size.
    let size = unsafe {
        getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };
    if size < 0 {
        bail!("Failed to getxattr: {}", io::Error::last_os_error());
    } else if size != SHA256_HASH_SIZE as isize {
        bail!("Unexpected hash size: {}", size);
    } else {
        Ok(buf)
    }
}
