package app.eggspot.eggdb;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.charset.StandardCharsets;
import java.sql.SQLException;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;

/**
 * One key-value transaction: a read over one snapshot, or a write that commits atomically. A
 * {@code collection} of {@code null} is the default key space. Closing it without
 * {@link #commit()} rolls it back.
 */
public final class EggDbTransaction implements AutoCloseable {
    private static final int SCAN_REVERSE = 1;
    private static final int SCAN_END_INCLUSIVE = 2;
    private static final int SCAN_PREFIX = 4;
    private static final long ENTRY_BYTES = 32;
    private static final long STATS_BYTES = 24;

    /** One key and its value from a scan. */
    public record Entry(byte[] key, byte[] value) {
    }

    /** What a collection holds. */
    public record CollectionStats(long entries, long keyBytes, long valueBytes) {
    }

    /**
     * A scan's range: keys from {@code start} up to {@code end} ({@code end} left out unless
     * {@code endInclusive}), or the keys beginning with {@code prefix}, in key order or reversed.
     */
    public record Range(byte[] start, byte[] end, byte[] prefix, boolean reverse, boolean endInclusive) {
        /** Every key. */
        public static Range all() {
            return new Range(null, null, null, false, false);
        }

        /** The keys from {@code start} up to, not including, {@code end}; either may be {@code null}. */
        public static Range between(byte[] start, byte[] end) {
            return new Range(start, end, null, false, false);
        }

        /** The keys beginning with {@code prefix}. */
        public static Range withPrefix(byte[] prefix) {
            return new Range(null, null, prefix, false, false);
        }

        /** The same range, last key first. */
        public Range reversed() {
            return new Range(start, end, prefix, !reverse, endInclusive);
        }

        /** The same range with its end key included. */
        public Range includingEnd() {
            return new Range(start, end, prefix, reverse, true);
        }
    }

    /** Called with each entry of a scan; returning {@code false} stops it. */
    @FunctionalInterface
    public interface Visitor {
        /** Sees one entry and says whether to go on. */
        boolean visit(byte[] key, byte[] value) throws SQLException;
    }

    private MemorySegment handle;
    private final boolean writing;

    EggDbTransaction(MemorySegment handle, boolean writing) {
        this.handle = handle;
        this.writing = writing;
    }

    /** Whether this is a write transaction. */
    public boolean isWriting() {
        return writing;
    }

    /** The UTF-8 bytes of {@code text}, for text keys and values. */
    public static byte[] bytes(String text) {
        return text.getBytes(StandardCharsets.UTF_8);
    }

    private MemorySegment live() throws SQLException {
        if (handle == null) {
            throw new SQLException("the transaction has ended", "25000");
        }
        return handle;
    }

    private static MemorySegment name(Arena arena, String collection) {
        return collection == null ? MemorySegment.NULL : arena.allocateFrom(collection);
    }

    private static MemorySegment copy(Arena arena, byte[] data) {
        if (data == null || data.length == 0) {
            return MemorySegment.NULL;
        }
        MemorySegment segment = arena.allocate(data.length);
        MemorySegment.copy(data, 0, segment, ValueLayout.JAVA_BYTE, 0, data.length);
        return segment;
    }

    private static long length(byte[] data) {
        return data == null ? 0 : data.length;
    }

    private static byte[] taken(MemorySegment data, long length) throws Throwable {
        byte[] copied = data.reinterpret(length).toArray(ValueLayout.JAVA_BYTE);
        KeyValueApi.FREE.invokeExact(data, length);
        return copied;
    }

    /** The value of {@code key}, or {@code null} when it is absent or has expired. */
    public byte[] get(String collection, byte[] key) throws SQLException {
        MemorySegment txn = live();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment data = arena.allocate(ValueLayout.ADDRESS);
            MemorySegment size = arena.allocate(ValueLayout.JAVA_LONG);
            KeyValueApi.check((int) KeyValueApi.GET.invokeExact(txn, name(arena, collection), copy(arena, key),
                    length(key), data, size));
            MemorySegment value = data.get(ValueLayout.ADDRESS, 0);
            if (value.equals(MemorySegment.NULL)) {
                return null;
            }
            return taken(value, size.get(ValueLayout.JAVA_LONG, 0));
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** Stores {@code value} under {@code key}. */
    public void put(String collection, byte[] key, byte[] value) throws SQLException {
        put(collection, key, value, null);
    }

    /** Stores {@code value} under {@code key}, expiring after {@code timeToLive} when it is given. */
    public void put(String collection, byte[] key, byte[] value, Duration timeToLive) throws SQLException {
        MemorySegment txn = live();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment collectionName = name(arena, collection);
            MemorySegment keyCopy = copy(arena, key);
            MemorySegment valueCopy = copy(arena, value);
            int status;
            if (timeToLive != null && !timeToLive.isNegative() && !timeToLive.isZero()) {
                status = (int) KeyValueApi.PUT_WITH_EXPIRY.invokeExact(txn, collectionName, keyCopy, length(key),
                        valueCopy, length(value), timeToLive.toMillis());
            } else {
                status = (int) KeyValueApi.PUT.invokeExact(txn, collectionName, keyCopy, length(key), valueCopy,
                        length(value));
            }
            KeyValueApi.check(status);
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** Removes {@code key} and says whether it was there. */
    public boolean delete(String collection, byte[] key) throws SQLException {
        MemorySegment txn = live();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment deleted = arena.allocate(ValueLayout.JAVA_INT);
            KeyValueApi.check((int) KeyValueApi.DELETE.invokeExact(txn, name(arena, collection), copy(arena, key),
                    length(key), deleted));
            return deleted.get(ValueLayout.JAVA_INT, 0) != 0;
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** Calls {@code visitor} with each entry of {@code range} in order until it returns {@code false}. */
    public void scan(String collection, Range range, Visitor visitor) throws SQLException {
        MemorySegment txn = live();
        int flags = (range.reverse() ? SCAN_REVERSE : 0) | (range.endInclusive() ? SCAN_END_INCLUSIVE : 0);
        byte[] start = range.start();
        byte[] end = range.end();
        if (range.prefix() != null) {
            flags |= SCAN_PREFIX;
            start = range.prefix();
            end = null;
        }
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
            KeyValueApi.check((int) KeyValueApi.SCAN_OPEN.invokeExact(txn, name(arena, collection), copy(arena, start),
                    length(start), copy(arena, end), length(end), flags, out));
            MemorySegment scan = out.get(ValueLayout.ADDRESS, 0);
            try {
                visitBatches(txn, scan, arena, visitor);
            } finally {
                KeyValueApi.SCAN_FREE.invokeExact(scan);
            }
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    private static void visitBatches(MemorySegment txn, MemorySegment scan, Arena arena, Visitor visitor)
            throws Throwable {
        MemorySegment entries = arena.allocate(ValueLayout.ADDRESS);
        MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
        while (true) {
            KeyValueApi.check((int) KeyValueApi.SCAN_NEXT.invokeExact(txn, scan, 0L, entries, count));
            long total = count.get(ValueLayout.JAVA_LONG, 0);
            if (total == 0) {
                return;
            }
            MemorySegment batch = entries.get(ValueLayout.ADDRESS, 0).reinterpret(ENTRY_BYTES * total);
            List<Entry> copied = new ArrayList<>((int) total);
            for (long index = 0; index < total; index++) {
                long at = index * ENTRY_BYTES;
                copied.add(new Entry(
                        bytesAt(batch.get(ValueLayout.ADDRESS, at), batch.get(ValueLayout.JAVA_LONG, at + 8)),
                        bytesAt(batch.get(ValueLayout.ADDRESS, at + 16), batch.get(ValueLayout.JAVA_LONG, at + 24))));
            }
            for (Entry entry : copied) {
                if (!visitor.visit(entry.key(), entry.value())) {
                    return;
                }
            }
        }
    }

    private static byte[] bytesAt(MemorySegment data, long length) {
        return length == 0 ? new byte[0] : data.reinterpret(length).toArray(ValueLayout.JAVA_BYTE);
    }

    /** Every entry of {@code range}, in order. */
    public List<Entry> scan(String collection, Range range) throws SQLException {
        List<Entry> entries = new ArrayList<>();
        scan(collection, range, (key, value) -> entries.add(new Entry(key, value)));
        return entries;
    }

    /** Creates the collection {@code name} and says whether it was made. */
    public boolean createCollection(String name) throws SQLException {
        return flagged(name, true);
    }

    /** Drops the collection {@code name} with its keys and says whether it was there. */
    public boolean dropCollection(String name) throws SQLException {
        return flagged(name, false);
    }

    private boolean flagged(String collection, boolean creating) throws SQLException {
        MemorySegment txn = live();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment flag = arena.allocate(ValueLayout.JAVA_INT);
            MemorySegment name = arena.allocateFrom(collection);
            int status = creating
                    ? (int) KeyValueApi.CREATE_COLLECTION.invokeExact(txn, name, flag)
                    : (int) KeyValueApi.DROP_COLLECTION.invokeExact(txn, name, flag);
            KeyValueApi.check(status);
            return flag.get(ValueLayout.JAVA_INT, 0) != 0;
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** Renames the collection {@code name} to {@code newName}, keeping its keys. */
    public void renameCollection(String name, String newName) throws SQLException {
        MemorySegment txn = live();
        try (Arena arena = Arena.ofConfined()) {
            KeyValueApi.check((int) KeyValueApi.RENAME_COLLECTION.invokeExact(txn, arena.allocateFrom(name),
                    arena.allocateFrom(newName)));
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** The collections' names, in name order. */
    public List<String> collections() throws SQLException {
        MemorySegment txn = live();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment data = arena.allocate(ValueLayout.ADDRESS);
            MemorySegment size = arena.allocate(ValueLayout.JAVA_LONG);
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            KeyValueApi.check((int) KeyValueApi.COLLECTION_NAMES.invokeExact(txn, data, size, count));
            MemorySegment names = data.get(ValueLayout.ADDRESS, 0);
            List<String> listed = new ArrayList<>();
            if (names.equals(MemorySegment.NULL)) {
                return listed;
            }
            String joined = new String(taken(names, size.get(ValueLayout.JAVA_LONG, 0)), StandardCharsets.UTF_8);
            long wanted = count.get(ValueLayout.JAVA_LONG, 0);
            for (String name : joined.split("\0", -1)) {
                if (listed.size() == wanted) {
                    break;
                }
                listed.add(name);
            }
            return listed;
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** What the collection {@code name} holds. */
    public CollectionStats collectionStats(String name) throws SQLException {
        MemorySegment txn = live();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment stats = arena.allocate(STATS_BYTES, 8);
            KeyValueApi.check((int) KeyValueApi.COLLECTION_STATS.invokeExact(txn, arena.allocateFrom(name), stats));
            return new CollectionStats(stats.get(ValueLayout.JAVA_LONG, 0), stats.get(ValueLayout.JAVA_LONG, 8),
                    stats.get(ValueLayout.JAVA_LONG, 16));
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** Commits the writes and returns the commit's stamp; the transaction ends either way. */
    public long commit() throws SQLException {
        MemorySegment txn = live();
        handle = null;
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment stamp = arena.allocate(ValueLayout.JAVA_LONG);
            KeyValueApi.check((int) KeyValueApi.COMMIT.invokeExact(txn, stamp));
            return stamp.get(ValueLayout.JAVA_LONG, 0);
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** Ends the transaction without committing; after {@link #commit()} it does nothing. */
    public void rollback() throws SQLException {
        if (handle == null) {
            return;
        }
        MemorySegment txn = handle;
        handle = null;
        try {
            KeyValueApi.check((int) KeyValueApi.ROLLBACK.invokeExact(txn));
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    @Override
    public void close() throws SQLException {
        rollback();
    }
}
