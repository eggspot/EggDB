package app.eggspot.eggdb;

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.nio.file.Path;
import java.sql.SQLException;

/**
 * The key-value store of an embedded EggDB file: transactions over its default key space and its
 * named collections. Close it when done; it is not safe to share between threads without
 * synchronization.
 */
public final class EggDbKeyValue implements AutoCloseable {
    private static final int OPEN_READ_ONLY = 1;

    /** Work done inside one transaction. */
    @FunctionalInterface
    public interface Work<T> {
        /** Runs with the transaction and returns the result handed back to the caller. */
        T run(EggDbTransaction transaction) throws SQLException;
    }

    private MemorySegment database;
    private final boolean readOnly;

    private EggDbKeyValue(MemorySegment database, boolean readOnly) {
        this.database = database;
        this.readOnly = readOnly;
    }

    /** Opens the key-value store of the database file at {@code path}, creating the file unless read-only. */
    public static EggDbKeyValue open(Path path, boolean readOnly) throws SQLException {
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment options = arena.allocate(16, 8);
            options.set(ValueLayout.JAVA_INT, 0, readOnly ? OPEN_READ_ONLY : 0);
            MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
            KeyValueApi.check((int) KeyValueApi.OPEN.invokeExact(arena.allocateFrom(path.toString()), options, out));
            return new EggDbKeyValue(out.get(ValueLayout.ADDRESS, 0), readOnly);
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** Whether the store was opened read-only. */
    public boolean isReadOnly() {
        return readOnly;
    }

    /** Begins a read transaction over one snapshot, or a write transaction; close or end it when done. */
    public EggDbTransaction begin(boolean writing) throws SQLException {
        if (database == null) {
            throw new SQLException("the store is closed", "08003");
        }
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
            int status = writing
                    ? (int) KeyValueApi.BEGIN_WRITE.invokeExact(database, out)
                    : (int) KeyValueApi.BEGIN_READ.invokeExact(database, out);
            KeyValueApi.check(status);
            return new EggDbTransaction(out.get(ValueLayout.ADDRESS, 0), writing);
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }

    /** Runs {@code work} in a read transaction and returns its result. */
    public <T> T read(Work<T> work) throws SQLException {
        try (EggDbTransaction transaction = begin(false)) {
            return work.run(transaction);
        }
    }

    /** Runs {@code work} in a write transaction, committing when it returns and rolling back when it throws. */
    public <T> T write(Work<T> work) throws SQLException {
        try (EggDbTransaction transaction = begin(true)) {
            T result = work.run(transaction);
            transaction.commit();
            return result;
        }
    }

    @Override
    public void close() throws SQLException {
        if (database == null) {
            return;
        }
        MemorySegment closing = database;
        database = null;
        try {
            KeyValueApi.check((int) KeyValueApi.CLOSE.invokeExact(closing));
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw KeyValueApi.unreachable(error);
        }
    }
}
