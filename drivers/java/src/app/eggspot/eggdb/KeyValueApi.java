package app.eggspot.eggdb;

import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.sql.SQLException;

final class KeyValueApi {
    static final MethodHandle OPEN;
    static final MethodHandle CLOSE;
    static final MethodHandle FREE;
    static final MethodHandle BEGIN_READ;
    static final MethodHandle BEGIN_WRITE;
    static final MethodHandle COMMIT;
    static final MethodHandle ROLLBACK;
    static final MethodHandle GET;
    static final MethodHandle PUT;
    static final MethodHandle PUT_WITH_EXPIRY;
    static final MethodHandle DELETE;
    static final MethodHandle SCAN_OPEN;
    static final MethodHandle SCAN_NEXT;
    static final MethodHandle SCAN_FREE;
    static final MethodHandle CREATE_COLLECTION;
    static final MethodHandle DROP_COLLECTION;
    static final MethodHandle RENAME_COLLECTION;
    static final MethodHandle COLLECTION_NAMES;
    static final MethodHandle COLLECTION_STATS;

    static {
        ValueLayout pointer = ValueLayout.ADDRESS;
        ValueLayout status = ValueLayout.JAVA_INT;
        ValueLayout size = ValueLayout.JAVA_LONG;
        OPEN = NativeLibrary.handle("eggdb_open", FunctionDescriptor.of(status, pointer, pointer, pointer));
        CLOSE = NativeLibrary.handle("eggdb_close", FunctionDescriptor.of(status, pointer));
        FREE = NativeLibrary.handle("eggdb_free", FunctionDescriptor.ofVoid(pointer, size));
        BEGIN_READ = NativeLibrary.handle("eggdb_begin_read", FunctionDescriptor.of(status, pointer, pointer));
        BEGIN_WRITE = NativeLibrary.handle("eggdb_begin_write", FunctionDescriptor.of(status, pointer, pointer));
        COMMIT = NativeLibrary.handle("eggdb_commit", FunctionDescriptor.of(status, pointer, pointer));
        ROLLBACK = NativeLibrary.handle("eggdb_rollback", FunctionDescriptor.of(status, pointer));
        GET = NativeLibrary.handle("eggdb_get", FunctionDescriptor.of(status, pointer, pointer, pointer, size, pointer, pointer));
        PUT = NativeLibrary.handle("eggdb_put", FunctionDescriptor.of(status, pointer, pointer, pointer, size, pointer, size));
        PUT_WITH_EXPIRY = NativeLibrary.handle("eggdb_put_with_expiry",
                FunctionDescriptor.of(status, pointer, pointer, pointer, size, pointer, size, ValueLayout.JAVA_LONG));
        DELETE = NativeLibrary.handle("eggdb_delete", FunctionDescriptor.of(status, pointer, pointer, pointer, size, pointer));
        SCAN_OPEN = NativeLibrary.handle("eggdb_scan_open",
                FunctionDescriptor.of(status, pointer, pointer, pointer, size, pointer, size, ValueLayout.JAVA_INT, pointer));
        SCAN_NEXT = NativeLibrary.handle("eggdb_scan_next", FunctionDescriptor.of(status, pointer, pointer, size, pointer, pointer));
        SCAN_FREE = NativeLibrary.handle("eggdb_scan_free", FunctionDescriptor.ofVoid(pointer));
        CREATE_COLLECTION = NativeLibrary.handle("eggdb_create_collection", FunctionDescriptor.of(status, pointer, pointer, pointer));
        DROP_COLLECTION = NativeLibrary.handle("eggdb_drop_collection", FunctionDescriptor.of(status, pointer, pointer, pointer));
        RENAME_COLLECTION = NativeLibrary.handle("eggdb_rename_collection", FunctionDescriptor.of(status, pointer, pointer, pointer));
        COLLECTION_NAMES = NativeLibrary.handle("eggdb_collection_names",
                FunctionDescriptor.of(status, pointer, pointer, pointer, pointer));
        COLLECTION_STATS = NativeLibrary.handle("eggdb_collection_stats", FunctionDescriptor.of(status, pointer, pointer, pointer));
    }

    private KeyValueApi() {
    }

    static void check(int status) throws SQLException {
        if (status != 0) {
            throw NativeSession.lastError(status);
        }
    }

    static SQLException unreachable(Throwable error) {
        return new SQLException("EggDB's C library could not be called: " + error, "58000", error);
    }
}
