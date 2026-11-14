package app.eggspot.eggdb;

import java.sql.SQLException;
import java.util.ArrayList;
import java.util.List;

final class SteppedRows {
    private final NativeStatement statement;
    private final int limit;
    private final List<Object[]> window = new ArrayList<>();
    private int windowStart;
    private boolean ended;

    SteppedRows(NativeStatement statement, int limit) {
        this.statement = statement;
        this.limit = limit;
    }

    boolean has(int index) throws SQLException {
        if (index < windowStart || (limit > 0 && index >= limit)) {
            return false;
        }
        while (!ended && windowStart + window.size() <= index) {
            Object[] row = statement.step();
            if (row == null) {
                ended = true;
                statement.reset();
            } else {
                window.add(row);
            }
        }
        while (windowStart < index && !window.isEmpty()) {
            window.remove(0);
            windowStart++;
        }
        return index < windowStart + window.size();
    }

    Object[] get(int index) {
        return window.get(index - windowStart);
    }

    List<Object[]> window() {
        return List.copyOf(window);
    }

    void close() {
        if (!ended) {
            ended = true;
            statement.reset();
        }
        window.clear();
    }
}
