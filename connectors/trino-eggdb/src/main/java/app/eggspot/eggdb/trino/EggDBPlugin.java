package app.eggspot.eggdb.trino;

import io.trino.plugin.jdbc.JdbcPlugin;
import io.trino.plugin.postgresql.PostgreSqlClientModule;

public final class EggDBPlugin extends JdbcPlugin {
    public EggDBPlugin() {
        super("eggdb", PostgreSqlClientModule::new);
    }
}
