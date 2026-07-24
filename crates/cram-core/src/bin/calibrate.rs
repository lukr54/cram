//! `calibrate` — auto-detect this machine and show the settings Cram's engine derives.
//!
//!   calibrate                 detect + light in-memory calibration + derived plans (no disk writes)
//!   calibrate --recalibrate   ignore any cached profile and re-run the micro-bench
//!   calibrate --write-probe    ALSO measure the true sustained write wall (writes ~4 GiB, then deletes)

use cram_core::hw::{
    self, Bottleneck, Bus, Codec, DriveInfo, HwProfile, Op, Plan, Shape, Topology,
};

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

fn bus_str(b: Bus) -> &'static str {
    match b {
        Bus::Nvme => "NVMe",
        Bus::Sata => "SATA",
        Bus::Usb => "USB",
        Bus::Other => "other",
        Bus::Unknown => "unknown",
    }
}

fn drive_str(d: &Option<DriveInfo>) -> String {
    match d {
        Some(di) => {
            let media = match di.ssd {
                Some(true) => "SSD",
                Some(false) => "HDD",
                None => "unknown media",
            };
            format!(
                "PhysicalDrive{} — {} ({})",
                di.number,
                media,
                bus_str(di.bus)
            )
        }
        None => "unknown (detection unavailable)".to_string(),
    }
}

fn plan_line(label: &str, p: &Plan) {
    let side = match p.bottleneck {
        Bottleneck::WriteBound => "WRITE-bound",
        Bottleneck::CpuBound => "CPU-bound",
    };
    let shape = match p.shape {
        Shape::PerEntry => "per-entry",
        Shape::Pipeline => "pipeline→1 writer",
        Shape::Serial => "serial",
    };
    println!(
        "  {label:<34} {side:<11} | {shape:<17} | {}w/{}wr | {}",
        p.workers, p.writers, p.note
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let recal = args.iter().any(|a| a == "--recalibrate");
    let write_probe = args.iter().any(|a| a == "--write-probe");
    let probe_gib: usize = args
        .iter()
        .position(|a| a == "--probe-gib")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(160);

    println!("Cram — hardware auto-detect & calibration\n");

    // ---- Layer 1: static profile ----
    let hw = HwProfile::detect();
    println!("Detected hardware:");
    println!(
        "  CPU: {} logical / {} physical  (SMT: {})",
        hw.logical,
        hw.physical,
        if hw.smt { "yes" } else { "no" }
    );
    println!(
        "  RAM: {:.1} GiB total, {:.1} GiB available",
        gib(hw.ram_total),
        gib(hw.ram_avail)
    );
    println!("  Working drive: {}", drive_str(&hw.work_drive));

    let default_wall = hw
        .work_drive
        .as_ref()
        .map(|d| d.default_wall_mibs())
        .unwrap_or(250.0);

    // ---- Layer 4: calibration (cached unless --recalibrate) ----
    let cached = if recal { None } else { hw::load_profile() };
    let (rates, mut wall, source) = match cached {
        Some((r, w)) if r.deflate_dec > 0.0 => {
            let wall = if w > 0.0 { w } else { default_wall };
            (r, wall, "cached profile")
        }
        _ => {
            print!("\nCalibrating codec throughput (in-memory, ~2s)... ");
            use std::io::Write;
            std::io::stdout().flush().ok();
            let r = hw::calibrate(64);
            println!("done");
            (r, default_wall, "measured now")
        }
    };

    println!("\nCalibrated per-core throughput ({source}):");
    println!(
        "  DEFLATE  decode {:>6.0} MiB/s   encode {:>5.0} MiB/s",
        rates.deflate_dec, rates.deflate_enc
    );
    println!("  LZMA/xz  decode {:>6.0} MiB/s", rates.lzma_dec);

    // ---- optional heavy write-wall probe ----
    let mut measured_wall = wall > 0.0 && source == "cached profile";
    if write_probe {
        let dir = std::env::current_dir().unwrap_or_else(|_| ".".into());
        println!(
            "\nMeasuring sustained write wall (writing up to {} GiB to {}, early-stops at the SLC cliff, then deletes)...",
            probe_gib,
            dir.display()
        );
        match hw::measure_write_wall(&dir, probe_gib * 1024) {
            Ok(w) => {
                println!(
                    "  burst {:.0} MiB/s | sustained {:.0} MiB/s | SLC cliff {}",
                    w.burst_mibs,
                    w.sustained_mibs,
                    w.cliff_mib
                        .map(|c| format!("~{:.0} GiB in", c / 1024.0))
                        .unwrap_or_else(|| format!(
                            "not reached within {probe_gib} GiB (SLC cache is larger)"
                        ))
                );
                wall = if w.sustained_mibs > 0.0 {
                    w.sustained_mibs
                } else {
                    wall
                };
                measured_wall = true;
            }
            Err(e) => println!("  write probe failed: {e}"),
        }
    }

    println!(
        "\nWrite wall: {:.0} MiB/s  ({})",
        wall,
        if measured_wall {
            "measured on your drive"
        } else {
            "ESTIMATE from bus/media — run `calibrate --write-probe` to measure; QLC/DRAM-less drives run far lower"
        }
    );

    // ---- Layers 2+3: derive the settings the engine would use ----
    let topo = Topology::SameDrive; // src=dst on the working drive for these examples
    println!("\nDerived settings (topology: same drive):");
    let plan = |op, codec, blocks| hw::derive_plan(op, codec, blocks, &hw, topo, &rates, wall);
    plan_line(
        "Extract big low-ratio ZIP",
        &plan(Op::Extract, Codec::Deflate, 5000),
    );
    plan_line(
        "Extract 7z (1 solid block)",
        &plan(Op::Extract, Codec::Lzma, 1),
    );
    plan_line(
        "Extract 7z (multi-block)",
        &plan(Op::Extract, Codec::Lzma, 64),
    );
    plan_line("Create .zip / .tar.zst", &plan(Op::Create, Codec::Zstd, 0));
    plan_line(
        "Create .7z / .tar.xz (LZMA2)",
        &plan(Op::Create, Codec::Lzma, 0),
    );

    // ---- persist ----
    match hw::save_profile(&rates, if measured_wall { Some(wall) } else { None }) {
        Ok(()) => {
            if let Some(p) = hw::profile_path() {
                println!("\nSaved profile → {}", p.display());
            }
        }
        Err(e) => println!("\n(could not save profile: {e})"),
    }
}
