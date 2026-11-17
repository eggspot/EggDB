# Metabase EggDB driver

A Metabase driver plugin for EggDB, built on Metabase's PostgreSQL driver (EggDB speaks the
PostgreSQL protocol). It adds **EggDB** to Metabase's database list, with a choice of the SQL
dialect native questions are written in.

Build it with Metabase's driver build (`clojure -X:build:drivers:build/driver :driver :eggdb`
from a Metabase checkout with this directory as `modules/drivers/eggdb`) and copy the jar into
Metabase's `plugins` directory.

Licensed under MIT OR Apache-2.0, like EggDB.
