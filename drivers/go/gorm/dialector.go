package eggdbgorm

import (
	"database/sql"
	"errors"
	"fmt"
	"strconv"
	"strings"

	eggdb "github.com/eggspot/EggDB/drivers/go"
	"gorm.io/gorm"
	"gorm.io/gorm/callbacks"
	"gorm.io/gorm/clause"
	"gorm.io/gorm/logger"
	"gorm.io/gorm/migrator"
	"gorm.io/gorm/schema"
)

// Dialector is EggDB's own GORM dialector: GORM's SQL in EggDB's SQLite dialect, run through the
// eggdb database/sql driver, with a migrator that reads EggDB's catalog. It links nothing of
// SQLite's.
type Dialector struct {
	DSN  string
	Conn gorm.ConnPool
}

// New returns EggDB's own dialector for the file at dsn (a path, or ":memory:", with the
// driver's "?mode=ro" setting).
func New(dsn string) gorm.Dialector {
	return &Dialector{DSN: dsn}
}

// Name is "sqlite": GORM and its plugins choose SQLite's SQL forms by this name, and EggDB speaks
// them.
func (Dialector) Name() string {
	return "sqlite"
}

// Initialize registers GORM's callbacks with RETURNING for created, updated and deleted rows,
// opens the connection pool and installs the LIMIT and FOR clause builders SQLite's SQL needs.
func (d *Dialector) Initialize(db *gorm.DB) error {
	callbacks.RegisterDefaultCallbacks(db, &callbacks.Config{
		CreateClauses:        []string{"INSERT", "VALUES", "ON CONFLICT", "RETURNING"},
		UpdateClauses:        []string{"UPDATE", "SET", "FROM", "WHERE", "RETURNING"},
		DeleteClauses:        []string{"DELETE", "FROM", "WHERE", "RETURNING"},
		LastInsertIDReversed: true,
	})
	if d.Conn != nil {
		db.ConnPool = d.Conn
	} else {
		pool, err := sql.Open(DriverName, d.DSN)
		if err != nil {
			return err
		}
		db.ConnPool = pool
	}
	db.ClauseBuilders["FOR"] = func(clause.Clause, clause.Builder) {}
	db.ClauseBuilders["LIMIT"] = func(c clause.Clause, builder clause.Builder) {
		limit, ok := c.Expression.(clause.Limit)
		if !ok {
			return
		}
		count := -1
		if limit.Limit != nil && *limit.Limit >= 0 {
			count = *limit.Limit
		}
		if count >= 0 || limit.Offset > 0 {
			builder.WriteString("LIMIT ")
			builder.WriteString(strconv.Itoa(count))
		}
		if limit.Offset > 0 {
			builder.WriteString(" OFFSET ")
			builder.WriteString(strconv.Itoa(limit.Offset))
		}
	}
	return nil
}

// Migrator returns the migrator that reads tables, columns and indexes from sqlite_master and
// PRAGMA table_info.
func (d Dialector) Migrator(db *gorm.DB) gorm.Migrator {
	return Migrator{migrator.Migrator{Config: migrator.Config{
		DB:                          db,
		Dialector:                   d,
		CreateIndexAfterCreateTable: true,
	}}}
}

// DataTypeOf is SQLite's type name for a field; an auto-increment integer is the table's
// INTEGER PRIMARY KEY AUTOINCREMENT.
func (Dialector) DataTypeOf(field *schema.Field) string {
	switch field.DataType {
	case schema.Bool:
		return "numeric"
	case schema.Int, schema.Uint:
		if field.AutoIncrement {
			return "integer PRIMARY KEY AUTOINCREMENT"
		}
		return "integer"
	case schema.Float:
		return "real"
	case schema.String:
		return "text"
	case schema.Time:
		return "datetime"
	case schema.Bytes:
		return "blob"
	}
	return string(field.DataType)
}

// DefaultValueOf is NULL, written where GORM inserts a row with no values.
func (Dialector) DefaultValueOf(*schema.Field) clause.Expression {
	return clause.Expr{SQL: "NULL"}
}

// BindVarTo writes a "?" placeholder.
func (Dialector) BindVarTo(writer clause.Writer, _ *gorm.Statement, _ interface{}) {
	writer.WriteByte('?')
}

// QuoteTo writes a name in backticks, each dotted part apart.
func (Dialector) QuoteTo(writer clause.Writer, name string) {
	for at, part := range strings.Split(name, ".") {
		if at > 0 {
			writer.WriteByte('.')
		}
		writer.WriteByte('`')
		writer.WriteString(strings.ReplaceAll(part, "`", "``"))
		writer.WriteByte('`')
	}
}

// Explain writes a statement with its values in place, for GORM's logger.
func (Dialector) Explain(text string, values ...interface{}) string {
	return logger.ExplainSQL(text, nil, `"`, values...)
}

// SavePoint starts a savepoint.
func (Dialector) SavePoint(tx *gorm.DB, name string) error {
	return tx.Exec("SAVEPOINT " + name).Error
}

// RollbackTo rolls back to a savepoint.
func (Dialector) RollbackTo(tx *gorm.DB, name string) error {
	return tx.Exec("ROLLBACK TO SAVEPOINT " + name).Error
}

// Translate turns EggDB's unique and foreign key violations into GORM's errors, when the
// configuration asks for TranslateError.
func (Dialector) Translate(err error) error {
	var reported *eggdb.Error
	if !errors.As(err, &reported) {
		return err
	}
	switch reported.SQLState {
	case "23505":
		return gorm.ErrDuplicatedKey
	case "23503":
		return gorm.ErrForeignKeyViolated
	}
	return err
}

// Migrator is GORM's migrator with the catalog reads EggDB's SQLite dialect answers.
type Migrator struct {
	migrator.Migrator
}

func (m Migrator) count(query string, values ...interface{}) int {
	var found int
	if err := m.DB.Raw(query, values...).Row().Scan(&found); err != nil {
		return 0
	}
	return found
}

// CurrentDatabase is "main", the one schema of a file.
func (Migrator) CurrentDatabase() string {
	return "main"
}

// HasTable reports whether the model's table exists.
func (m Migrator) HasTable(value interface{}) bool {
	found := 0
	_ = m.RunWithValue(value, func(stmt *gorm.Statement) error {
		found = m.count("SELECT count(*) FROM sqlite_master WHERE type = ? AND name = ?", "table", stmt.Table)
		return nil
	})
	return found > 0
}

// GetTables lists the tables, not SQLite's own.
func (m Migrator) GetTables() ([]string, error) {
	var tables []string
	err := m.DB.Raw("SELECT name FROM sqlite_master WHERE type = ? AND name NOT LIKE ?", "table", "sqlite_%").Scan(&tables).Error
	return tables, err
}

// HasColumn reports whether the model's table has a column, by field or column name.
func (m Migrator) HasColumn(value interface{}, name string) bool {
	found := false
	_ = m.RunWithValue(value, func(stmt *gorm.Statement) error {
		if stmt.Schema != nil {
			if field := stmt.Schema.LookUpField(name); field != nil {
				name = field.DBName
			}
		}
		quoted := strings.ReplaceAll(stmt.Table, "`", "``")
		rows, err := m.DB.Raw(fmt.Sprintf("PRAGMA table_info(`%s`)", quoted)).Rows()
		if err != nil {
			return err
		}
		defer rows.Close()
		columns, err := rows.Columns()
		if err != nil {
			return err
		}
		for rows.Next() {
			cells := make([]interface{}, len(columns))
			names := make([]sql.NullString, len(columns))
			for at := range cells {
				cells[at] = &names[at]
			}
			if err := rows.Scan(cells...); err != nil {
				return err
			}
			for at, column := range columns {
				if column == "name" && strings.EqualFold(names[at].String, name) {
					found = true
				}
			}
		}
		return rows.Err()
	})
	return found
}

// HasIndex reports whether the model's table has an index, by its GORM or SQL name.
func (m Migrator) HasIndex(value interface{}, name string) bool {
	found := 0
	_ = m.RunWithValue(value, func(stmt *gorm.Statement) error {
		if stmt.Schema != nil {
			if index := stmt.Schema.LookIndex(name); index != nil {
				name = index.Name
			}
		}
		found = m.count("SELECT count(*) FROM sqlite_master WHERE type = ? AND tbl_name = ? AND name = ?", "index", stmt.Table, name)
		return nil
	})
	return found > 0
}

// DropIndex drops an index by its GORM or SQL name; SQLite's DROP INDEX names no table.
func (m Migrator) DropIndex(value interface{}, name string) error {
	return m.RunWithValue(value, func(stmt *gorm.Statement) error {
		if stmt.Schema != nil {
			if index := stmt.Schema.LookIndex(name); index != nil {
				name = index.Name
			}
		}
		return m.DB.Exec("DROP INDEX ?", clause.Column{Name: name}).Error
	})
}

// RenameIndex renames an index by creating it again under the new name, as SQLite has no
// ALTER INDEX.
func (m Migrator) RenameIndex(value interface{}, oldName, newName string) error {
	return m.RunWithValue(value, func(stmt *gorm.Statement) error {
		var definition string
		if err := m.DB.Raw("SELECT sql FROM sqlite_master WHERE type = ? AND tbl_name = ? AND name = ?", "index", stmt.Table, oldName).Row().Scan(&definition); err != nil {
			return err
		}
		if err := m.DB.Exec("DROP INDEX ?", clause.Column{Name: oldName}).Error; err != nil {
			return err
		}
		return m.DB.Exec(strings.Replace(definition, oldName, newName, 1)).Error
	})
}
