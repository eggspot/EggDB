package app.eggspot.eggdb.hibernate;

import org.hibernate.community.dialect.SQLiteDialect;
import org.hibernate.dialect.DatabaseVersion;
import org.hibernate.engine.jdbc.dialect.spi.DialectResolutionInfo;

/**
 * Hibernate's dialect for an embedded EggDB file reached through EggDB's JDBC driver
 * ({@code jdbc:eggdb:app.db}). EggDB speaks SQLite's SQL there, so this is Hibernate's own SQLite
 * dialect at the SQLite language level EggDB answers to.
 */
public final class EggDbDialect extends SQLiteDialect {
    /** The SQLite language level EggDB's SQLite dialect speaks. */
    public static final DatabaseVersion SQLITE_LANGUAGE = DatabaseVersion.make(3, 45);

    /** The dialect at EggDB's SQLite language level. */
    public EggDbDialect() {
        super(SQLITE_LANGUAGE);
    }

    /** The dialect Hibernate makes from a connection's metadata; EggDB's version does not change it. */
    public EggDbDialect(DialectResolutionInfo info) {
        this();
    }
}
