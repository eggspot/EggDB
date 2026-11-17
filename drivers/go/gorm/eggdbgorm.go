// Package eggdbgorm opens an embedded EggDB file with GORM, through EggDB's own dialector: GORM's
// SQL in EggDB's SQLite dialect, run by EggDB's database/sql driver, with nothing of SQLite's
// linked in.
package eggdbgorm

import (
	"gorm.io/gorm"
)

// DriverName is the database/sql driver EggDB's Go package registers.
const DriverName = "eggdb"

// Open returns a GORM dialector for the EggDB file at dsn (a path, or ":memory:", with the
// driver's "?mode=ro" setting), in EggDB's SQLite dialect.
func Open(dsn string) gorm.Dialector {
	return New(dsn)
}
