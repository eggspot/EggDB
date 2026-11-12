// Package eggdbgorm opens an embedded EggDB file with GORM. EggDB speaks SQLite's SQL, so GORM's
// own SQLite dialector does the SQL, migrations and schema reflection, and EggDB's database/sql
// driver runs it in place of SQLite's.
package eggdbgorm

import (
	_ "github.com/eggspot/EggDB/drivers/go"
	"gorm.io/driver/sqlite"
	"gorm.io/gorm"
)

// DriverName is the database/sql driver EggDB's Go package registers.
const DriverName = "eggdb"

// Open returns a GORM dialector for the EggDB file at dsn (a path, or ":memory:", with the
// driver's "?mode=ro" setting), in EggDB's SQLite dialect.
func Open(dsn string) gorm.Dialector {
	return sqlite.New(sqlite.Config{DriverName: DriverName, DSN: dsn})
}
