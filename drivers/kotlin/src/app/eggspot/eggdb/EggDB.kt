package app.eggspot.eggdb

import java.nio.charset.StandardCharsets.UTF_8

/** A value in a result row. */
sealed interface Value {
    /** SQL's NULL. */
    data object Null : Value

    /** A 64-bit integer. */
    data class Integer(val value: Long) : Value

    /** A double-precision number. */
    data class Real(val value: Double) : Value

    /** Text. */
    data class Text(val value: String) : Value

    /** Bytes. */
    class Blob(val value: ByteArray) : Value {
        override fun equals(other: Any?): Boolean = other is Blob && other.value.contentEquals(value)

        override fun hashCode(): Int = value.contentHashCode()
    }
}

/** What one statement returned: its command tag, column names and rows. */
data class Result(val tag: String, val columns: List<String>, val rows: List<List<Value>>)

/** A statement or the database failed; the message is EggDB's, with the SQLSTATE when there is one. */
class EggDBException(message: String) : Exception(message)

/**
 * An embedded EggDB database opened at [path], running SQL in [dialect] (`sqlite`, `postgresql`,
 * `mysql`, `sqlserver` or `oracle`), on Android, on any JVM, or anywhere the C API runs.
 * Not safe to share between threads without synchronization; close it when done.
 */
class EggDB(path: String, val dialect: String = "sqlite") : AutoCloseable {
    private var handle: Long = nativeOpen(path.toByteArray(UTF_8), dialect.toByteArray(UTF_8))

    /** Runs the statements in [sql] and returns one [Result] per statement. */
    fun execute(sql: String): List<Result> {
        check(handle != 0L) { "the database is closed" }
        val flat = nativeExecute(handle, sql.toByteArray(UTF_8))
        val results = mutableListOf<Result>()
        var at = 0
        while (at < flat.size) {
            val tag = String(flat[at++]!!, UTF_8)
            val columnCount = String(flat[at++]!!, UTF_8).toInt()
            val rowCount = String(flat[at++]!!, UTF_8).toInt()
            val columns = List(columnCount) { String(flat[at++]!!, UTF_8) }
            val rows = List(rowCount) { List(columnCount) { decode(flat[at++]) } }
            results += Result(tag, columns, rows)
        }
        return results
    }

    override fun close() {
        if (handle != 0L) {
            nativeClose(handle)
            handle = 0L
        }
    }

    private fun decode(cell: ByteArray?): Value {
        if (cell == null) return Value.Null
        val payload = cell.copyOfRange(1, cell.size)
        return when (cell[0].toInt()) {
            1 -> Value.Integer(String(payload, UTF_8).toLong())
            2 -> Value.Real(String(payload, UTF_8).toDouble())
            3 -> Value.Text(String(payload, UTF_8))
            else -> Value.Blob(payload)
        }
    }

    companion object {
        init {
            System.loadLibrary("eggdb_jni")
        }

        @JvmStatic
        private external fun nativeOpen(path: ByteArray, dialect: ByteArray): Long

        @JvmStatic
        private external fun nativeExecute(handle: Long, sql: ByteArray): Array<ByteArray?>

        @JvmStatic
        private external fun nativeClose(handle: Long)
    }
}
