package app.eggspot.eggdb

import java.nio.charset.StandardCharsets.UTF_8
import kotlin.time.Duration

/** What a collection holds. */
data class CollectionStats(val entries: Long, val keyBytes: Long, val valueBytes: Long)

/** One key and its value from a scan. */
class Entry(val key: ByteArray, val value: ByteArray) {
    override fun equals(other: Any?): Boolean =
        other is Entry && other.key.contentEquals(key) && other.value.contentEquals(value)

    override fun hashCode(): Int = 31 * key.contentHashCode() + value.contentHashCode()
}

/**
 * The key-value store of an embedded EggDB file at [path]: transactions over its default key
 * space and its named collections. Not safe to share between threads without synchronization;
 * close it when done.
 */
class KeyValue(path: String, val readOnly: Boolean = false) : AutoCloseable {
    private var handle: Long = nativeOpen(path.toByteArray(UTF_8), readOnly)

    /** Begins a read transaction over one snapshot, or a write transaction when [writing]. */
    fun begin(writing: Boolean): Transaction {
        if (handle == 0L) throw EggDBException("the store is closed")
        return Transaction(nativeBegin(handle, writing), writing)
    }

    /** Runs [work] in a read transaction and returns its result. */
    fun <T> read(work: (Transaction) -> T): T = begin(false).use(work)

    /** Runs [work] in a write transaction, committing when it returns and rolling back when it throws. */
    fun <T> write(work: (Transaction) -> T): T =
        begin(true).use { transaction ->
            val result = work(transaction)
            transaction.commit()
            result
        }

    /**
     * Gives the file up while the app is in the background, from `onStop`: checkpoints, syncs
     * and releases the file's lock; transactions fail until [resume].
     */
    fun suspend() {
        if (handle == 0L) throw EggDBException("the store is closed")
        nativeSuspend(handle)
    }

    /** Takes the file back, from `onStart`, after [suspend]. */
    fun resume() {
        if (handle == 0L) throw EggDBException("the store is closed")
        nativeResume(handle)
    }

    /** Tells the store what the device runs on, so it checkpoints less often on a battery. */
    fun setPowerSource(source: PowerSource) {
        if (handle == 0L) throw EggDBException("the store is closed")
        nativeSetPowerSource(handle, source.code)
    }

    override fun close() {
        if (handle != 0L) {
            val closing = handle
            handle = 0L
            nativeClose(closing)
        }
    }

    companion object {
        init {
            System.loadLibrary("eggdb_jni")
        }

        @JvmStatic
        private external fun nativeOpen(path: ByteArray, readOnly: Boolean): Long

        @JvmStatic
        private external fun nativeClose(handle: Long)

        @JvmStatic
        private external fun nativeBegin(handle: Long, writing: Boolean): Long

        @JvmStatic
        private external fun nativeSuspend(handle: Long)

        @JvmStatic
        private external fun nativeResume(handle: Long)

        @JvmStatic
        private external fun nativeSetPowerSource(handle: Long, source: Int)
    }
}

/** What the device runs on: Android's `BatteryManager` and power-save mode say which. */
enum class PowerSource(internal val code: Int) {
    MAINS(0),
    BATTERY(1),
    LOW_BATTERY(2),
}

/**
 * One key-value transaction: a read over one snapshot, or a write that commits atomically. A
 * `null` collection is the default key space. Closing it without [commit] rolls it back.
 */
class Transaction internal constructor(private var handle: Long, val writing: Boolean) : AutoCloseable {
    private fun live(): Long {
        if (handle == 0L) throw EggDBException("the transaction has ended")
        return handle
    }

    /** The value of [key], or `null` when it is absent or has expired. */
    fun get(key: ByteArray, collection: String? = null): ByteArray? = nativeGet(live(), collection?.toByteArray(UTF_8), key)

    /** The value of the text [key] as text, or `null`. */
    fun getText(key: String, collection: String? = null): String? = get(key.toByteArray(UTF_8), collection)?.toString(UTF_8)

    /** Stores [value] under [key], expiring after [timeToLive] when it is given. */
    fun put(key: ByteArray, value: ByteArray, collection: String? = null, timeToLive: Duration? = null) {
        val milliseconds = timeToLive?.inWholeMilliseconds?.coerceAtLeast(1) ?: 0L
        nativePut(live(), collection?.toByteArray(UTF_8), key, value, milliseconds)
    }

    /** Stores the text [value] under the text [key]. */
    fun put(key: String, value: String, collection: String? = null, timeToLive: Duration? = null) =
        put(key.toByteArray(UTF_8), value.toByteArray(UTF_8), collection, timeToLive)

    /** Removes [key] and says whether it was there. */
    fun delete(key: ByteArray, collection: String? = null): Boolean = nativeDelete(live(), collection?.toByteArray(UTF_8), key)

    /**
     * The entries from [start] up to [end] ([end] left out unless [endInclusive]), or those whose
     * keys begin with [prefix], in key order or reversed, at most [limit] of them when it is above zero.
     */
    fun scan(
        start: ByteArray? = null,
        end: ByteArray? = null,
        prefix: ByteArray? = null,
        reverse: Boolean = false,
        endInclusive: Boolean = false,
        limit: Int = 0,
        collection: String? = null,
    ): List<Entry> {
        var flags = (if (reverse) SCAN_REVERSE else 0) or (if (endInclusive) SCAN_END_INCLUSIVE else 0)
        var from = start
        var to = end
        if (prefix != null) {
            flags = flags or SCAN_PREFIX
            from = prefix
            to = null
        }
        val flat = nativeScan(live(), collection?.toByteArray(UTF_8), from, to, flags, limit)
        return List(flat.size / 2) { index -> Entry(flat[2 * index], flat[2 * index + 1]) }
    }

    /** Creates the collection [name] and says whether it was made. */
    fun createCollection(name: String): Boolean = nativeCreateCollection(live(), name.toByteArray(UTF_8))

    /** Drops the collection [name] with its keys and says whether it was there. */
    fun dropCollection(name: String): Boolean = nativeDropCollection(live(), name.toByteArray(UTF_8))

    /** Renames the collection [name] to [newName], keeping its keys. */
    fun renameCollection(name: String, newName: String) =
        nativeRenameCollection(live(), name.toByteArray(UTF_8), newName.toByteArray(UTF_8))

    /** The collections' names, in name order. */
    fun collections(): List<String> = nativeCollections(live()).map { it.toString(UTF_8) }

    /** What the collection [name] holds. */
    fun collectionStats(name: String): CollectionStats {
        val counts = nativeStats(live(), name.toByteArray(UTF_8))
        return CollectionStats(counts[0], counts[1], counts[2])
    }

    /** Commits the writes and returns the commit's stamp; the transaction ends either way. */
    fun commit(): Long {
        val committing = live()
        handle = 0L
        return nativeCommit(committing)
    }

    /** Ends the transaction without committing; after [commit] it does nothing. */
    fun rollback() {
        if (handle != 0L) {
            val ending = handle
            handle = 0L
            nativeRollback(ending)
        }
    }

    override fun close() = rollback()

    private companion object {
        const val SCAN_REVERSE = 1
        const val SCAN_END_INCLUSIVE = 2
        const val SCAN_PREFIX = 4

        @JvmStatic
        external fun nativeCommit(txn: Long): Long

        @JvmStatic
        external fun nativeRollback(txn: Long)

        @JvmStatic
        external fun nativeGet(txn: Long, collection: ByteArray?, key: ByteArray): ByteArray?

        @JvmStatic
        external fun nativePut(txn: Long, collection: ByteArray?, key: ByteArray, value: ByteArray, ttlMs: Long)

        @JvmStatic
        external fun nativeDelete(txn: Long, collection: ByteArray?, key: ByteArray): Boolean

        @JvmStatic
        external fun nativeScan(txn: Long, collection: ByteArray?, start: ByteArray?, end: ByteArray?, flags: Int, limit: Int): Array<ByteArray>

        @JvmStatic
        external fun nativeCreateCollection(txn: Long, name: ByteArray): Boolean

        @JvmStatic
        external fun nativeDropCollection(txn: Long, name: ByteArray): Boolean

        @JvmStatic
        external fun nativeRenameCollection(txn: Long, name: ByteArray, newName: ByteArray)

        @JvmStatic
        external fun nativeCollections(txn: Long): Array<ByteArray>

        @JvmStatic
        external fun nativeStats(txn: Long, name: ByteArray): LongArray
    }
}
