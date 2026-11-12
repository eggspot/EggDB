package app.eggspot.eggdb.exposed

import org.jetbrains.exposed.sql.Database
import org.jetbrains.exposed.sql.vendors.SQLiteDialect

/**
 * Exposed on an embedded EggDB file through EggDB's JDBC driver: `jdbc:eggdb:` URLs use
 * Exposed's SQLite dialect, the SQL EggDB speaks for JDBC connections.
 */
object EggDbExposed {
    /** The JDBC driver class of EggDB's Java driver. */
    const val DRIVER = "app.eggspot.eggdb.EggDbDriver"

    /** The URL prefix of EggDB's JDBC driver. */
    const val URL_PREFIX = "jdbc:eggdb"

    /** Tells Exposed to use its SQLite dialect for `jdbc:eggdb:` URLs; safe to call more than once. */
    fun register() {
        Database.registerJdbcDriver(URL_PREFIX, DRIVER, SQLiteDialect.dialectName)
    }

    /** Connects Exposed to the EggDB file at [path] (or `:memory:`). */
    fun connect(path: String): Database {
        register()
        return Database.connect("$URL_PREFIX:$path", driver = DRIVER)
    }
}
