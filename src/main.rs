#![feature(popcorn_protocol)]
#![feature(maybe_uninit_array_assume_init)]
#![feature(bstr)]
#![feature(core_io_borrowed_buf)]

use std::bstr::ByteStr;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::{Debug, Formatter};
use std::io::BorrowedBuf;
use std::mem::MaybeUninit;
use aml::{AmlContext, AmlValue, DebugVerbosity, LevelType};
use std::os::popcorn::handle::{AsHandle, AsRawHandle, BorrowedHandle};
use std::os::popcorn::proto::io::{Read, ReadTr};
use std::os::popcorn::proto::proc::{Thread, ThreadTr};
use std::ptr::{addr_of, NonNull};
use std::fmt::Write;
use std::os::popcorn::ffi::OsStrExt;
use std::os::popcorn::process::CommandExt;
use std::slice;
use std::sync::Arc;
use acpi::{AcpiTables, AmlTable, PhysicalMapping};
use log::{debug, info, Level, trace, warn};
use popcorn_server::SyncTr as _;
use proto::client::BusNodeTr as _;

mod server;

#[repr(C, packed)]
struct Rsdp {
    signature: [u8; 8],
    checksum: u8,
    oem_id: [u8; 6],
    revision: u8,
    rsdt_address: u32,

    /*
     * These fields are only valid for ACPI Version 2.0 and greater
     */
    length: u32,
    xsdt_address: u64,
    ext_checksum: u8,
    reserved: [u8; 3],
}

impl Debug for Rsdp {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Rsdp");
        d.field("signature", &ByteStr::new(&self.signature));
        d.field("checksum", &self.checksum);
        d.field("oem_id", &ByteStr::new(&self.oem_id));
        d.field("revision", &self.revision);
        if self.revision == 0 {
            let rsdt_address = self.rsdt_address;
            d.field("rsdt_address", &rsdt_address);
            d.finish()
        } else {
            let length = self.length;
            let xsdt_address = self.xsdt_address;
            d.field("length", &length);
            d.field("xsdt_address", &xsdt_address);
            d.field("ext_checksum", &self.ext_checksum);
            d.finish()
        }
    }
}

impl Rsdp {
    fn address(&self) -> usize {
        if self.revision == 0 { self.rsdt_address as _ }
        else { self.xsdt_address as _ }
    }
}

#[repr(C, packed)]
struct TableHeader {
    signature: [u8; 4],
    length: u32,
    revision: u8,
    checksum: u8,
    oem_id: [u8; 6],
    oem_table_id: [u8; 8],
    oem_revision: u32,
    creator_id: u32,
    creator_revision: u32,
}

fn main() {
    println!("starting ia32_acpi_root driver...");

    simple_logger::init_with_level(Level::Debug).unwrap();

    let rsdp = std::os::popcorn::env::get_handle::<Read>("popcorn.init.root-bus-descriptor")
            .expect("root bus descriptor handle not found");
    let thread = std::os::popcorn::env::get_handle::<Thread>("thread.main")
            .expect("main thread handle not found");
    let bus = std::os::popcorn::env::get_handle::<proto::client::BusNode>("driver.node")
            .expect("device handle not found");

    debug!("rsdp handle at {}", rsdp.as_raw_handle().0);

    let mut raw = [const { MaybeUninit::<u8>::uninit() }; const { size_of::<Rsdp>() }];
    let mut buf = BorrowedBuf::from(&mut raw[..]);
    assert_eq!(rsdp.read(buf.unfilled()).unwrap(), size_of::<Rsdp>());
    drop(buf);
    let rsdp = unsafe { std::mem::transmute::<_, Rsdp>(MaybeUninit::array_assume_init(raw)) };

    trace!("{rsdp:#x?}");


    let tables = unsafe { AcpiTables::from_rsdt(AcpiHandler(thread), rsdp.revision, rsdp.address()) }.unwrap();
    let dsdt = tables.dsdt().unwrap();
    let ssdts = tables.ssdts();

    let mut ctx = AmlContext::new(Box::new(AmlHandler), DebugVerbosity::Scopes);

    let mut parse_table = |table: AmlTable| {
        let aligned = table.address & !0xfff;
        let diff = table.address - aligned;

        let total_size = table.length as usize + diff;

        let data = unsafe {
            slice::from_raw_parts(
                thread.unstable_mmio_alloc(aligned, total_size)
                    .expect("failed to map table")
                    .byte_add(diff),
                table.length as usize,
            )
        };

        ctx.parse_table(data).expect("parsing table failed");
    };

    parse_table(dsdt);
    for table in ssdts {
        parse_table(table);
    }

    trace!("acpi namespace: {:#?}", ctx.namespace);

    let mut device_hids = vec![];
    let _ = ctx.namespace.traverse(|name, level| {
        if level.typ == LevelType::Device {
            debug!("found device {name:?}");
            for (name, handle) in level.values.iter() {
                if name.as_str() == "_HID" {
                    device_hids.push(*handle);
                    continue;
                }
            }
        }
        Ok(true)
    });

    let srv = popcorn_server::Server::new(
        ":",
        |handle| server::Server::new(handle, tables)
    ).expect("failed to start acpi server");
    let acpi_handle = srv.handle().as_handle().forge::<proto::client::Acpi>(1).unwrap();

    let mut launched_ps2 = false;
    for hid in device_hids {
        let eisa = match ctx.namespace.get(hid) {
            Ok(AmlValue::Integer(val)) => {
                let eisa = parse_eisa_id(*val);
                info!("found device with HID `{eisa}` (`{val}`)");
                eisa
            }
            Ok(AmlValue::String(val)) => {
                info!("found device with HID `{val}`");
                val.clone()
            }
            Ok(_) => {
                warn!("ignoring device because hid was not string/integer");
                continue;
            }
            Err(e) => {
                warn!("ignoring device - error {e:?}");
                continue;
            }
        };

        match &*eisa {
            "PNP0103" => { /* HPET */ },
            "PNP0501" => { /* 16550A UART */ },
            "PNP0303" | "PNP0F13" => {
                /* PS/2 keyboard and mouse */
                if launched_ps2 { continue; }
                launched_ps2 = true;

                let handle = bus.create_child("ps2").expect("failed to create child device node");
                handle.try_combine_with(
                    acpi_handle.try_clone().expect("failed to clone acpi handle")
                ).expect("failed to attach acpi handle");

                std::process::Command::new("fs:/system/bin/driver/i8042.exec")
                        .stdin(std::process::Stdio::null())
                        .handle(
                            OsStr::from_str("driver.node").to_owned(),
                            std::os::popcorn::process::inherit_from(handle)
                        )
                        .spawn();
            },
            "PNP0B00" => { /* PC RTC */ },
            "PNP0A03" | "PNP0A08" => {
                /* PCI bus */
                let handle = bus.create_child("pci").expect("failed to create child device node");
                handle.try_combine_with(
                    acpi_handle.try_clone().expect("failed to clone acpi handle")
                ).expect("failed to attach acpi handle");

                std::process::Command::new("fs:/system/bin/driver/pci.exec")
                        .stdin(std::process::Stdio::null())
                        .handle(
                            OsStr::from_str("driver.node").to_owned(),
                            std::os::popcorn::process::inherit_from(handle)
                        )
                        .spawn();
            },
            _ => debug!("Unknown device")
        }
    }

    Arc::new(srv).run();
}

struct AmlHandler;

impl aml::Handler for AmlHandler {
    fn read_u8(&self, address: usize) -> u8 {
        todo!()
    }

    fn read_u16(&self, address: usize) -> u16 {
        todo!()
    }

    fn read_u32(&self, address: usize) -> u32 {
        todo!()
    }

    fn read_u64(&self, address: usize) -> u64 {
        todo!()
    }

    fn write_u8(&mut self, address: usize, value: u8) {
        todo!()
    }

    fn write_u16(&mut self, address: usize, value: u16) {
        todo!()
    }

    fn write_u32(&mut self, address: usize, value: u32) {
        todo!()
    }

    fn write_u64(&mut self, address: usize, value: u64) {
        todo!()
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        todo!()
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        todo!()
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        todo!()
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        todo!()
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        todo!()
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        todo!()
    }

    fn read_pci_u8(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
        todo!()
    }

    fn read_pci_u16(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
        todo!()
    }

    fn read_pci_u32(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        todo!()
    }

    fn write_pci_u8(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u8) {
        todo!()
    }

    fn write_pci_u16(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u16) {
        todo!()
    }

    fn write_pci_u32(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16, value: u32) {
        todo!()
    }
}

#[derive(Clone, Copy)]
struct AcpiHandler(BorrowedHandle<'static, Thread>);

impl acpi::AcpiHandler for AcpiHandler {
    unsafe fn map_physical_region<T>(&self, physical_address: usize, size: usize) -> PhysicalMapping<Self, T> {
        let aligned = physical_address & !0xfff;
        let diff = physical_address - aligned;

        let total_size = size + diff;

        let virtual_start = unsafe {
            NonNull::new_unchecked(
                self.0.unstable_mmio_alloc(aligned, total_size)
                      .expect("failed to map DSDT")
                      .byte_add(diff)
                      .cast::<T>()
            )
        };

        unsafe {
            PhysicalMapping::new(
                physical_address,
                virtual_start,
                size,
                total_size,
                *self
            )
        }
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {

    }
}

fn parse_eisa_id(val: u64) -> String {
    let val = (val as u32).to_be(); // todo: is this correct on BE systems
    let a = ((val >> 26) & 0x1f) as u8 + 0x40;
    let b = ((val >> 21) & 0x1f) as u8 + 0x40;
    let c = ((val >> 16) & 0x1f) as u8 + 0x40;
    let mut buf = String::new();
    buf.push(a as char);
    buf.push(b as char);
    buf.push(c as char);
    let _ = write!(&mut buf, "{:04X}", val as u16);
    buf
}
