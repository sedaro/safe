## Temporary Steps

To configure an EDS for integration
```
pull lastest scf
bash setup.sh
SIM_UPLOAD_ARTIFACT=0 bash run.sh satops
cd simulation/scenario_latest
vim main to add `--release` to `cargo run` command
./main --target-config /Users/sebastianwelsh/Development/sedaro/scf/results/20251210_183535 --duration 1
```

After doing so, find the .bin file that was created for init
```
ls -al simulation/data # find latest file
```

Update references to a `.bin` file in the main.rs code.