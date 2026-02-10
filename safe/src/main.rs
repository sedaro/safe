mod c2;
mod config;
mod definitions;
mod kits;
mod observability;
mod router;
mod transports;
mod simulation;
mod flight;
mod utils;
mod modes;

use anyhow::Result;

use simvm::sv::data::Data;
use simvm::sv::{combine::TR, combine::TRD, parse::Parse};
use simvm::sv::ser_de::{dyn_de, dyn_ser};
use crate::simulation::{FileTargetReader, SedaroSimulator};
use crate::kits::stats::{GuassianSet, NormalDistribution, StatisticalDistribution};
use simvm::sv::update::Update;
use simvm::sv::pretty::Pretty;
use simvm::sv::check::Check;


#[tokio::main]
async fn main() -> Result<()> {

    // Setup
    let agent_id = "FpKnj2S4YcchDdf2BGX8cfn";
    let eds_path = std::path::PathBuf::from("/Users/sebastianwelsh/Downloads/simulation");
    let simulator = SedaroSimulator::new(
      &eds_path
    ).venv(
      std::path::PathBuf::from("/Users/sebastianwelsh/Development/sedaro/scf/.venv")
    ).debug();
    let results_path = std::path::PathBuf::from(format!("/Users/sebastianwelsh/Development/sedaro/scf/results/optimization_run"));

    // Initialize distributions for sampling patches
    let mut inertia_mat_0_0_dist = NormalDistribution::new(0.000005, 0.000005 * ((5.0 / 3.0) / 100.0)).seed(10);
    let mut inertia_mat_1_1_dist = NormalDistribution::new(0.000005, 0.000005 * ((5.0 / 3.0) / 100.0)).seed(11);
    let mut inertia_mat_2_2_dist = NormalDistribution::new(0.000005, 0.000005 * ((5.0 / 3.0) / 100.0)).seed(12);

    // Load init data
    let type_sig = std::fs::read(eds_path.join(format!("data/init_ty_{agent_id}.json")))?;
    let type_sig_str = std::str::from_utf8(&type_sig)?;
    let init_type = TR::parse(type_sig_str).unwrap();
    let init_file_path = eds_path.join(format!("data/init_{agent_id}.bin"));
    let init_bytes = std::fs::read(&init_file_path)?;
    let init_val = dyn_de(&init_type.typ, &init_bytes).unwrap();
    let init_val = TRD::from((init_type.clone(), init_val));
    println!("Original simulation input Datum: {:?}", init_val.pretty());
    println!("===============================================================");

    // Apply patches to update init data
    let patch_str = format!("(((({:.15}, {:.15}, {:.15}, {:.15}),),) : (gnc: (\"root!.pid_config\": (float, float, float, float),),))", 5e-5, 0.0, 2.5e-4, 0.01);
    let patch_trd = TRD::parse(&patch_str).unwrap();
    patch_trd.refn.check(&patch_trd.data).unwrap(); // Gives helpful error if patch is malformed
    let init_val = init_val.update(&patch_trd).unwrap();
    
    let patch_str = format!("((({:.15}, {:.15}, {:.15}),) : (gnc: (\"PTnxx2jZ8vzTJlRwjxyctn.inertia\": float, \"PTnxxBv6drdQlzqJlVvvbC.inertia\": float, \"PTnxxMb7DrlvQKWQsDST4r.inertia\": float),))", 0.005, 0.005, 0.005);
    let patch_trd = TRD::parse(&patch_str).unwrap();
    patch_trd.refn.check(&patch_trd.data).unwrap(); // Gives helpful error if patch is malformed
    let init_val = init_val.update(&patch_trd).unwrap();
    
    let patch_str = format!("((((({:.15}, 0, 0), (0, {:.15}, 0), (0, 0, {:.15})),),) : (gnc: (\"root!.inertia\": {{((float, float, float), (float, float, float), (float, float, float)) | #}},),))", inertia_mat_0_0_dist.sample(), inertia_mat_1_1_dist.sample(), inertia_mat_2_2_dist.sample());
    let patch_trd = TRD::parse(&patch_str).unwrap();
    // patch_trd.refn.check(&patch_trd.data).unwrap(); // BUG
    let init_val = init_val.update(&patch_trd).unwrap();
    println!("Modified simulation input Datum: {:?}", init_val.pretty());

    // Write modified initial data back to file
    let init_val = init_val.data;
    let bytes = dyn_ser(&init_type.typ, &init_val).unwrap();
    std::fs::write(&init_file_path, bytes)?;

    // Clear results dir and run simulation
    if results_path.exists() {
      tokio::fs::remove_dir_all(&results_path).await.ok();
    }
    let result = simulator.run(1.0, &results_path).await;
    match &result {
      Ok(output) => {
        match output.status.success() {
          true => {
            println!("Simulation completed successfully");
          },
          false => {
            return Err(anyhow::anyhow!("Simulation failed with non-zero exit code: {:?}", String::from_utf8_lossy(&output.stderr)));
          },
        }
      },
      Err(e) => {
        return Err(anyhow::anyhow!("Simulation failed: {:?}", e));
      },
    }

    // Extract frames from local target
    let mut reader = FileTargetReader::try_from_path(
      &results_path.join(format!("{agent_id}.gnc.jsonl"))
    ).await.unwrap();
    let frames = reader.read_frames().await.unwrap();
    
    // Post-process frames
    let mut observations = GuassianSet::new();
    for frame in &frames {
      let speed = frame.get_by_field("PTnxx2jZ8vzTJlRwjxyctn.speed").unwrap().data.as_f64().unwrap();
      observations.add(speed);
    }        
    println!("{}", observations);

    Ok(())
}