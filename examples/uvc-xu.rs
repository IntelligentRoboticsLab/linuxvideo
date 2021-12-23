//! Queries UVC Extension Units (XUs).

use std::{env, path::Path};

use livid::{uvc::UvcExt, Device};

fn usage() -> String {
    format!("usage: uvc-xu <device> <extension unit ID>")
}

fn main() -> livid::Result<()> {
    env_logger::init();

    let mut args = env::args_os().skip(1);

    let path = args.next().ok_or_else(usage)?;
    let unit_id = args.next().ok_or_else(usage)?;
    let unit_id: u8 = unit_id
        .to_str()
        .ok_or_else(|| format!("unit ID must be an integer"))?
        .parse()?;

    let device = Device::open(Path::new(&path))?;

    let uvc = UvcExt::new(&device);
    let xu = uvc.extension_unit(unit_id);

    for sel in 0..=0xff {
        let res = xu.control_info(sel);
        println!("{:#04x}: {:?}", sel, res);
    }

    Ok(())
}
