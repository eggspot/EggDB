package app.eggspot.eggdb;

import java.sql.SQLException;
import java.sql.Savepoint;

record EggDbSavepoint(int id, String name) implements Savepoint {
    String identifier() {
        return "eggdb_savepoint_" + id;
    }

    @Override
    public int getSavepointId() throws SQLException {
        if (name != null) {
            throw new SQLException("a named savepoint has no id");
        }
        return id;
    }

    @Override
    public String getSavepointName() throws SQLException {
        if (name == null) {
            throw new SQLException("an unnamed savepoint has no name");
        }
        return name;
    }
}
