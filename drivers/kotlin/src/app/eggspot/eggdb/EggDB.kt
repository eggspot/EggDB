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
        return unflatten(nativeExecute(handle, sql.toByteArray(UTF_8)))
    }

    /**
     * Prepares one statement with `?` (or `$1`) parameters, to run many times with
     * [Statement.execute] or [Statement.executeBatch]; close it when done.
     */
    fun prepare(sql: String): Statement {
        check(handle != 0L) { "the database is closed" }
        return Statement(nativePrepare(handle, sql.toByteArray(UTF_8)))
    }

    /** Begins a transaction; [commit] or [rollback] ends it. */
    fun beginTransaction() {
        execute("BEGIN")
    }

    /** Commits the open transaction. */
    fun commit() {
        execute("COMMIT")
    }

    /** Rolls the open transaction back. */
    fun rollback() {
        execute("ROLLBACK")
    }

    /** Runs [work] in a transaction: commits when it returns, rolls back when it throws. */
    fun <T> transaction(work: (EggDB) -> T): T {
        beginTransaction()
        val result = try {
            work(this)
        } catch (failure: Throwable) {
            runCatching { rollback() }
            throw failure
        }
        commit()
        return result
    }

    override fun close() {
        if (handle != 0L) {
            nativeClose(handle)
            handle = 0L
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

        @JvmStatic
        private external fun nativePrepare(handle: Long, sql: ByteArray): Long
    }
}

internal fun unflatten(flat: Array<ByteArray?>): List<Result> {
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

/**
 * A prepared statement of an [EggDB]. Parameters are `null`, [Long], [Int], [Double], [Float],
 * [Boolean] (as 1 or 0), [String], [ByteArray] or a [Value]; each run crosses into the database
 * once, whatever the number of parameters or rows.
 */
class Statement internal constructor(private var handle: Long) : AutoCloseable {
    /** Runs the statement once with [parameters] and returns its results. */
    fun execute(vararg parameters: Any?): List<Result> = executeBatch(listOf(parameters.asList()))

    /**
     * Runs the statement once per row of [rows], all in one crossing, and returns the last
     * run's results; wrap it in a transaction to commit the rows together.
     */
    fun executeBatch(rows: List<List<Any?>>): List<Result> {
        check(handle != 0L) { "the statement is closed" }
        val width = rows.firstOrNull()?.size ?: 0
        require(rows.all { it.size == width }) { "every row needs the same number of parameters" }
        val cells = rows.size * width
        val kinds = IntArray(cells)
        val integers = LongArray(cells)
        val reals = DoubleArray(cells)
        val bytes = arrayOfNulls<ByteArray>(cells)
        var at = 0
        for (row in rows) {
            for (parameter in row) {
                when (val value = if (parameter is Value) parameter.unwrapped() else parameter) {
                    null -> kinds[at] = NULL_KIND
                    is Long -> { kinds[at] = INTEGER_KIND; integers[at] = value }
                    is Int -> { kinds[at] = INTEGER_KIND; integers[at] = value.toLong() }
                    is Boolean -> { kinds[at] = INTEGER_KIND; integers[at] = if (value) 1 else 0 }
                    is Double -> { kinds[at] = REAL_KIND; reals[at] = value }
                    is Float -> { kinds[at] = REAL_KIND; reals[at] = value.toDouble() }
                    is String -> { kinds[at] = TEXT_KIND; bytes[at] = value.toByteArray(UTF_8) }
                    is ByteArray -> { kinds[at] = BLOB_KIND; bytes[at] = value }
                    else -> throw IllegalArgumentException("a ${value::class.simpleName} cannot be a parameter")
                }
                at++
            }
        }
        return unflatten(nativeRun(handle, rows.size, kinds, integers, reals, bytes))
    }

    override fun close() {
        if (handle != 0L) {
            nativeFinalize(handle)
            handle = 0L
        }
    }

    private fun Value.unwrapped(): Any? = when (this) {
        Value.Null -> null
        is Value.Integer -> value
        is Value.Real -> value
        is Value.Text -> value
        is Value.Blob -> value
    }

    companion object {
        private const val NULL_KIND = 0
        private const val INTEGER_KIND = 1
        private const val REAL_KIND = 2
        private const val TEXT_KIND = 3
        private const val BLOB_KIND = 4

        @JvmStatic
        private external fun nativeRun(handle: Long, rows: Int, kinds: IntArray, integers: LongArray, reals: DoubleArray, bytes: Array<ByteArray?>): Array<ByteArray?>

        @JvmStatic
        private external fun nativeFinalize(handle: Long)
    }
}
