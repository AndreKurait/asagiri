import java.sql.DriverManager;
import java.time.Duration;
import org.testcontainers.DockerClientFactory;
import org.testcontainers.containers.GenericContainer;
import org.testcontainers.containers.MySQLContainer;
import org.testcontainers.containers.wait.strategy.Wait;
import org.testcontainers.images.builder.Transferable;
import org.testcontainers.utility.DockerImageName;
import org.testcontainers.utility.TestcontainersConfiguration;

class AsagiriTestcontainers {
    private static void require(boolean value, String message) {
        if (!value) throw new AssertionError(message);
    }

    public static void main(String[] args) throws Exception {
        require(args.length == 2, "Pass the pinned Alpine and MySQL image references");
        require(!TestcontainersConfiguration.getInstance().isDisableChecks(), "Startup checks must remain enabled");
        require(!"true".equalsIgnoreCase(System.getenv("TESTCONTAINERS_RYUK_DISABLED")), "Ryuk must remain enabled");
        var client = DockerClientFactory.instance().client();
        require(client.versionCmd().exec().getVersion().split("\\.")[0].matches("[1-9][0-9]*"),
                "The engine must advertise a Docker-compatible version");

        try (var readiness = new GenericContainer<>(DockerImageName.parse(args[0]))
                .withCopyToContainer(Transferable.of("asagiri-sdk-copy"), "/tmp/asagiri-sdk-copy")
                .withCommand("sh", "-c", "echo asagiri-ready-first; sleep 1; echo asagiri-ready-second; sleep 240")
                .waitingFor(Wait.forLogMessage(".*asagiri-ready-.*\\s", 2))
                .withStartupTimeout(Duration.ofSeconds(45))) {
            readiness.start();
            var logs = readiness.getLogs();
            require(logs.contains("asagiri-ready-first") && logs.contains("asagiri-ready-second"),
                    "A streaming wait must observe both delayed readiness lines");
            var result = readiness.execInContainer("cat", "/tmp/asagiri-sdk-copy");
            require(result.getExitCode() == 0 && result.getStdout().equals("asagiri-sdk-copy"),
                    "The SDK must copy files before start and exec after readiness");
            System.out.println("PASS Testcontainers default startup checks, image defaults, copy, exec and two-line log wait");
        }

        // Testcontainers 1.20.3 requires an explicit compatibility declaration
        // for this official MySQL reference containing both a tag and a digest.
        require(args[1].startsWith("mysql:8.0@sha256:"), "Use pinned official MySQL 8.0");
        try (var mysql = new MySQLContainer<>(DockerImageName.parse(args[1]).asCompatibleSubstituteFor("mysql"))
                .withDatabaseName("asagiri_test")
                .withUsername("asagiri")
                .withPassword(java.util.UUID.randomUUID().toString())
                .waitingFor(Wait.forLogMessage(".*ready for connections.*\\s", 2))
                .withStartupTimeout(Duration.ofMinutes(3))) {
            mysql.start();
            require(mysql.getLogs().split("ready for connections", -1).length >= 3,
                    "MySQL must report readiness twice");
            try (var connection = DriverManager.getConnection(mysql.getJdbcUrl(), mysql.getUsername(), mysql.getPassword());
                 var statement = connection.createStatement();
                 var result = statement.executeQuery("SELECT 40 + 2")) {
                require(result.next() && result.getInt(1) == 42, "The published MySQL port must serve a real JDBC query");
            }
            System.out.println("PASS MySQL 8.0 double readiness and real JDBC query through the published port");
        }

        // Deliberately leave one resource to the standard Ryuk reaper. The
        // Python parent requires its removal after this JVM exits.
        var abandoned = new GenericContainer<>(DockerImageName.parse(args[0])).withCommand("sleep", "240");
        abandoned.start();
        var reapers = client.listContainersCmd().exec().stream()
                .filter(container -> container.getImage().startsWith("testcontainers/ryuk")).count();
        require(reapers > 0, "A real Ryuk reaper must be running");
        System.out.println("ASAGIRI_TESTCONTAINERS_RESOURCE " + abandoned.getContainerId());
    }
}
