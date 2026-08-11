# Historical Integration Notes

This file is retained as a historical pointer, not as a reproducible workflow.
The former notes referenced an external SCF repository, private host paths,
`setup.sh`, simulation scenarios, and generated `.bin` artifacts that are not
included in this repository.

For the current local SAFE workflow, run `./run.sh` from the repository root and
use the documentation under [`safe/docs/`](./safe/docs/).

If EDS integration work resumes, record the following before adding a new
recipe:

- External repository names and revisions.
- Required host tools and operating-system assumptions.
- Exact artifact-generation commands.
- How generated artifacts are copied or referenced by SAFE.
- A checked-in fixture or an explicit statement that the workflow requires
  external infrastructure.
