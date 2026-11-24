package dev.frontcap.input;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;

/** Journal safety checks; native dispatch qualification is separate. */
public final class InputFilesTest {
    interface Attempt { void run() throws Exception; }
    static void refuses(Attempt attempt) throws Exception {
        try { attempt.run(); } catch (IOException expected) { return; }
        throw new AssertionError("Expected refusal");
    }
    static void equal(String expected, String actual) {
        if (!expected.equals(actual)) throw new AssertionError(actual);
    }
    public static void main(String[] args) throws Exception {
        Path root = Files.createTempDirectory("frontcap-input-files-");
        try {
            refuses(() -> new InputFiles(root.resolve("unbounded"), 900_001));
            refuses(() -> new InputFiles(root.resolve("empty-lifetime"), 0));
            Path journal = root.resolve("owner");
            InputFiles owner = new InputFiles(journal, 5000);
            owner.publish("ready.json", "{\"generation\":\"one\"}");
            String request = "{\"request_id\":1}";
            Files.writeString(journal.resolve("request-1.json"), request);
            equal(request, owner.await(1));
            equal(request, Files.readString(journal.resolve("accepted-1.json")));
            // A changed request cannot replace the original admitted payload or be redispatched.
            Files.writeString(journal.resolve("request-1.json"), "{\"request_id\":2}");
            refuses(() -> owner.await(1));
            equal(request, Files.readString(journal.resolve("accepted-1.json")));
            owner.publish("reply-1.json", "{\"status\":\"injected\"}");
            refuses(() -> owner.publish("reply-1.json", "{}"));
            equal("{\"status\":\"injected\"}", Files.readString(journal.resolve("reply-1.json")));
            refuses(() -> new InputFiles(journal, 5000));
            Files.writeString(journal.resolve("request-2.json"), "x".repeat(8193));
            refuses(() -> owner.await(2));
            if (Files.exists(journal.resolve("accepted-2.json"))) throw new AssertionError("Oversize admitted");
            Files.createSymbolicLink(journal.resolve("request-3.json"), journal.resolve("request-1.json"));
            refuses(() -> owner.await(3));
            InputFiles expired = new InputFiles(root.resolve("expired"), 1);
            Thread.sleep(5);
            refuses(() -> expired.await(1));
            System.out.println("Input journal safety checks passed");
        } finally {
            try (java.util.stream.Stream<Path> paths = Files.walk(root)) {
                for (Path path : paths.sorted(java.util.Comparator.reverseOrder()).toArray(Path[]::new))
                    Files.delete(path);
            }
        }
    }
}
