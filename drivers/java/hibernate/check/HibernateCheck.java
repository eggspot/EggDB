import app.eggspot.eggdb.hibernate.EggDbDialect;
import jakarta.persistence.Entity;
import jakarta.persistence.GeneratedValue;
import jakarta.persistence.Id;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import org.hibernate.SessionFactory;
import org.hibernate.cfg.Configuration;

public final class HibernateCheck {
    @Entity(name = "Egg")
    public static class Egg {
        @Id
        @GeneratedValue
        Long id;

        String colour;

        protected Egg() {
        }

        Egg(String colour) {
            this.colour = colour;
        }
    }

    public static void main(String[] arguments) throws Exception {
        Path directory = Files.createTempDirectory("eggdb-hibernate-check");
        Configuration configuration = new Configuration()
                .addAnnotatedClass(Egg.class)
                .setProperty("hibernate.connection.url", "jdbc:eggdb:" + directory.resolve("app.db"))
                .setProperty("hibernate.dialect", EggDbDialect.class.getName())
                .setProperty("hibernate.hbm2ddl.auto", "create");
        try (SessionFactory factory = configuration.buildSessionFactory()) {
            factory.inTransaction(session -> {
                session.persist(new Egg("brown"));
                session.persist(new Egg("white"));
            });
            List<String> colours = factory.fromSession(session -> session
                    .createSelectionQuery("select colour from Egg order by id", String.class)
                    .getResultList());
            if (!colours.equals(List.of("brown", "white"))) {
                throw new IllegalStateException("hibernate check failed: " + colours);
            }
        }
        System.out.println("hibernate check: passed");
    }
}
