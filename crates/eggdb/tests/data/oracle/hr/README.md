# Oracle's HR sample schema scripts

These four scripts are Oracle's own, committed unchanged as test data for `eggdb restore
--dialect oracle` (roadmap item 43, decision D127). They are not part of the `eggdb` binary or
crate.

- Source: <https://github.com/oracle-samples/db-sample-schemas>, folder `human_resources/`
- Commit: `6660bad68c07bd143430ace58565b3f727e17263` (schema version 21)
- Files: `hr_install.sql`, `hr_create.sql`, `hr_populate.sql`, `hr_code.sql`
- Licence: MIT, Copyright (c) 2023 Oracle; each file keeps its notice in its `rem` header.

The installer's own verification query states the counts the restore is checked against:
regions 5, countries 25, departments 27, locations 23, employees 107, jobs 19, job_history 10.
