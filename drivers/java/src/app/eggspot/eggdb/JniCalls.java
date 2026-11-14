package app.eggspot.eggdb;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

final class JniCalls implements NativeCalls {
    private static final int COPY_PIECE_BYTES = 64 * 1024;

    static {
        String given = System.getProperty("eggdb.jni", System.getenv("EGGDB_JNI_LIBRARY"));
        if (given != null && !given.isEmpty()) {
            System.load(given);
        } else {
            System.loadLibrary("eggdb_jdbc");
        }
    }

    private static native long nativeOpen(byte[] path, boolean readOnly);

    private static native long nativeSqlOpen(long database, byte[] dialect);

    private static native long nativeConnect(byte[] url);

    private static native void nativeSqlClose(long session);

    private static native void nativeClose(long database);

    private static native byte[][] nativeExecute(long session, byte[] sql, byte[][] parameters);

    private static native long nativePrepare(long session, byte[] sql);

    private static native int nativeParameterCount(long statement);

    private static native void nativeBind(long statement, int[] kinds, long[] integers, double[] reals, byte[][] bytes);

    private static native byte[][] nativeRun(long statement);

    private static native byte[][] nativeColumns(long statement);

    private static native byte[][] nativeStep(long statement);

    private static native void nativeReset(long statement);

    private static native void nativeFinalize(long statement);

    private static native long nativeCopyInBegin(long session, byte[] sql);

    private static native long nativeCopyOutBegin(long session, byte[] sql);

    private static native void nativeCopyPut(long copy, byte[] data, int length);

    private static native byte[] nativeCopyRead(long copy);

    private static native byte[][] nativeCopyEnd(long copy);

    private static native void nativeCopyFree(long copy);

    private static byte[] utf8(String text) {
        return text.getBytes(StandardCharsets.UTF_8);
    }

    private static <T> T called(java.util.concurrent.Callable<T> call) throws SQLException {
        try {
            return call.call();
        } catch (EggDbNativeException failure) {
            throw failure.asSqlException();
        } catch (SQLException failure) {
            throw failure;
        } catch (Exception failure) {
            throw new SQLException("EggDB's JNI binding failed: " + failure, "58000", failure);
        }
    }

    @Override
    public long open(String path, boolean readOnly) throws SQLException {
        return called(() -> nativeOpen(utf8(path), readOnly));
    }

    @Override
    public long sqlOpen(long database, String dialect) throws SQLException {
        return called(() -> nativeSqlOpen(database, utf8(dialect)));
    }

    @Override
    public long connect(String url) throws SQLException {
        return called(() -> nativeConnect(utf8(url)));
    }

    @Override
    public void sqlClose(long session) {
        nativeSqlClose(session);
    }

    @Override
    public void close(long database) {
        nativeClose(database);
    }

    @Override
    public List<NativeSession.StatementResult> execute(long session, String sql, List<String> parameters) throws SQLException {
        byte[][] texts = new byte[parameters.size()][];
        for (int at = 0; at < texts.length; at++) {
            String text = parameters.get(at);
            texts[at] = text == null ? null : utf8(text);
        }
        return unflatten(called(() -> nativeExecute(session, utf8(sql), texts)));
    }

    @Override
    public long prepare(long session, String sql) throws SQLException {
        return called(() -> nativePrepare(session, utf8(sql)));
    }

    @Override
    public int parameterCount(long statement) throws SQLException {
        return called(() -> nativeParameterCount(statement));
    }

    @Override
    public void bind(long statement, Bindings bindings) throws SQLException {
        called(() -> {
            nativeBind(statement, bindings.kinds(), bindings.integers(), bindings.reals(), bindings.bytes());
            return null;
        });
    }

    @Override
    public List<NativeSession.StatementResult> run(long statement) throws SQLException {
        return unflatten(called(() -> nativeRun(statement)));
    }

    @Override
    public List<String> columns(long statement) throws SQLException {
        byte[][] names = called(() -> nativeColumns(statement));
        List<String> columns = new ArrayList<>(names.length);
        for (byte[] name : names) {
            columns.add(new String(name, StandardCharsets.UTF_8));
        }
        return columns;
    }

    @Override
    public Object[] step(long statement) throws SQLException {
        byte[][] cells = called(() -> nativeStep(statement));
        if (cells == null) {
            return null;
        }
        Object[] row = new Object[cells.length];
        for (int at = 0; at < cells.length; at++) {
            row[at] = decode(cells[at]);
        }
        return row;
    }

    @Override
    public void reset(long statement) {
        nativeReset(statement);
    }

    @Override
    public void finalizeStatement(long statement) {
        nativeFinalize(statement);
    }

    @Override
    public long copyIn(long session, String sql, InputStream source) throws SQLException {
        long copy = called(() -> nativeCopyInBegin(session, utf8(sql)));
        try {
            byte[] buffer = new byte[COPY_PIECE_BYTES];
            int read;
            while ((read = source.read(buffer)) > 0) {
                int length = read;
                called(() -> {
                    nativeCopyPut(copy, buffer, length);
                    return null;
                });
            }
            return NativeSession.copiedRows(unflatten(called(() -> nativeCopyEnd(copy))));
        } catch (IOException error) {
            throw new SQLException("reading the COPY data failed: " + error.getMessage(), "58030", error);
        } finally {
            nativeCopyFree(copy);
        }
    }

    @Override
    public void copyOut(long session, String sql, OutputStream sink) throws SQLException {
        long copy = called(() -> nativeCopyOutBegin(session, utf8(sql)));
        try {
            while (true) {
                byte[] piece = called(() -> nativeCopyRead(copy));
                if (piece == null || piece.length == 0) {
                    break;
                }
                sink.write(piece);
            }
            called(() -> nativeCopyEnd(copy));
        } catch (IOException error) {
            throw new SQLException("writing the COPY data failed: " + error.getMessage(), "58030", error);
        } finally {
            nativeCopyFree(copy);
        }
    }

    private static List<NativeSession.StatementResult> unflatten(byte[][] flat) {
        List<NativeSession.StatementResult> results = new ArrayList<>();
        int at = 0;
        while (at < flat.length) {
            String tag = new String(flat[at++], StandardCharsets.UTF_8);
            int columnCount = Integer.parseInt(new String(flat[at++], StandardCharsets.US_ASCII));
            int rowCount = Integer.parseInt(new String(flat[at++], StandardCharsets.US_ASCII));
            List<String> columns = new ArrayList<>(columnCount);
            for (int column = 0; column < columnCount; column++) {
                columns.add(new String(flat[at++], StandardCharsets.UTF_8));
            }
            List<Object[]> rows = new ArrayList<>(rowCount);
            for (int row = 0; row < rowCount; row++) {
                Object[] cells = new Object[columnCount];
                for (int column = 0; column < columnCount; column++) {
                    cells[column] = decode(flat[at++]);
                }
                rows.add(cells);
            }
            results.add(new NativeSession.StatementResult(tag, columns, rows));
        }
        return results;
    }

    private static Object decode(byte[] cell) {
        if (cell == null) {
            return null;
        }
        byte[] payload = Arrays.copyOfRange(cell, 1, cell.length);
        return switch (cell[0]) {
            case Bindings.INTEGER -> Long.parseLong(new String(payload, StandardCharsets.US_ASCII));
            case Bindings.REAL -> Double.parseDouble(new String(payload, StandardCharsets.US_ASCII));
            case Bindings.TEXT -> new String(payload, StandardCharsets.UTF_8);
            default -> payload;
        };
    }
}
