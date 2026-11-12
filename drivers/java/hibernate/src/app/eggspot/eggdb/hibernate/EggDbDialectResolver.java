package app.eggspot.eggdb.hibernate;

import org.hibernate.dialect.Dialect;
import org.hibernate.engine.jdbc.dialect.spi.DialectResolutionInfo;
import org.hibernate.engine.jdbc.dialect.spi.DialectResolver;

/** Picks {@link EggDbDialect} for connections whose database names itself EggDB. */
public final class EggDbDialectResolver implements DialectResolver {
    @Override
    public Dialect resolveDialect(DialectResolutionInfo info) {
        return "EggDB".equals(info.getDatabaseName()) ? new EggDbDialect(info) : null;
    }
}
