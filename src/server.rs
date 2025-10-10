use std::os::popcorn::handle::OwnedHandle;
use std::os::popcorn::proto::Error;
use std::path::Path;
use std::slice;
use std::sync::OnceLock;
use acpi::AcpiTables;
use acpi::mcfg::Mcfg;
use executor::io::popcorn::AsyncOwnedHandle;
use log::debug;
use popcorn_server::{CtorContext, DispatchTable, ProtocolVisitor, ReturnHandle, ServerHandler};

pub struct Server {
	handle: AsyncOwnedHandle<popcorn_server::Sync>,
	tables: AcpiTables<super::AcpiHandler>,
}

unsafe impl Sync for Server {}

impl Server {
	pub fn new(handle: AsyncOwnedHandle<popcorn_server::Sync>, tables: AcpiTables<super::AcpiHandler>) -> Self {
		Self { handle, tables }
	}
}

impl ServerHandler for Server {
	type CtorContext = CtorCtx;

	async fn ctor(&self, endpoint: &Path, ctx: Self::CtorContext) -> Result<ReturnHandle, Error> {
		Err(Error::UnsupportedProtocol)
	}

	async fn destroy(&self, handle: isize) -> Result<(), Error> {
		Err(Error::UnsupportedProtocol)
	}

	fn dispatch_table(&self) -> &'static DispatchTable {
		static DISPATCH: OnceLock<DispatchTable> = OnceLock::new();

		DISPATCH.get_or_init(|| DispatchTable::new()
				.add_vtable(<Self as proto::server::Acpi>::__vtable())
		)
	}

	fn handle(&self) -> &AsyncOwnedHandle<popcorn_server::Sync> {
		&self.handle
	}
}

impl proto::server::Acpi for Server {
	async fn new_from(&self, _: &Path, _: OwnedHandle) -> Result<ReturnHandle, Error> {
		Err(Error::UnsupportedProtocol)
	}

	async fn read_table(&self, handle: isize, _output_size: usize, signature: &str) -> Result<Box<[u8]>, Error> {
		debug_assert_eq!(handle, 1, "only handle 1 exists");
		debug!("request table `{signature}`");

		let bytes = match signature {
			"MCFG" => {
				let table = self.tables.find_table::<Mcfg>().map_err(|_| Error::InvalidName)?;
				let start = table.virtual_start().cast::<u8>();
				Box::<[_]>::from(
					unsafe { slice::from_raw_parts(start.as_ptr().cast_const(), table.region_length()) }
				)
			},
			_ => return Err(Error::InvalidName),
		};
		Ok(bytes)
	}
}

#[derive(Default)]
pub struct CtorCtx;

impl CtorContext for CtorCtx {
	fn visitors(&self) -> &'static ProtocolVisitor<Self> {
		static VISITOR: OnceLock<ProtocolVisitor<CtorCtx>> = OnceLock::new();
		VISITOR.get_or_init(|| ProtocolVisitor::new())
	}
}
