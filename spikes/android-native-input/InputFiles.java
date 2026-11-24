package dev.frontcap.input;

import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;

/** Exclusive producer lifetime. Existing journals never authorize a replacement driver. */
final class InputFiles {
    private final Path directory;
    private final long deadline;

    InputFiles(Path directory, long lifetimeMillis) throws IOException {
        if (!directory.isAbsolute() || lifetimeMillis <= 0 || lifetimeMillis > 900_000)
            throw new IOException("Absolute fresh directory and bounded lifetime required");
        Files.createDirectory(directory);
        this.directory = directory.toRealPath();
        deadline = System.nanoTime() + lifetimeMillis * 1_000_000L;
    }

    void publish(String name, String json) throws IOException {
        Path destination = directory.resolve(name);
        if (Files.exists(destination, LinkOption.NOFOLLOW_LINKS))
            throw new IOException("Original producer evidence already exists");
        Path temporary = directory.resolve(name + ".pending");
        Files.write(temporary, json.getBytes(StandardCharsets.UTF_8),
            java.nio.file.StandardOpenOption.CREATE_NEW, java.nio.file.StandardOpenOption.WRITE);
        Files.move(temporary, destination, StandardCopyOption.ATOMIC_MOVE);
    }

    String await(int sequence) throws Exception {
        Path request = directory.resolve("request-" + sequence + ".json");
        while (!Files.exists(request, LinkOption.NOFOLLOW_LINKS)) {
            if (System.nanoTime() >= deadline) throw new IOException("Input driver lifetime expired");
            Thread.sleep(10);
        }
        if (System.nanoTime() >= deadline) throw new IOException("Input driver lifetime expired");
        if (!Files.isRegularFile(request, LinkOption.NOFOLLOW_LINKS))
            throw new IOException("Input request must be a regular file");
        byte[] bytes;
        try (InputStream input = Files.newInputStream(request, LinkOption.NOFOLLOW_LINKS)) {
            bytes = input.readNBytes(8193);
        }
        if (bytes.length == 0 || bytes.length > 8192)
            throw new IOException("Input request size outside bounds");
        String value = new String(bytes, StandardCharsets.UTF_8);
        // Persist the exact admitted payload before parsing or any native effect. Lost replies
        // can only be reconciled against this attempt, never by resubmitting its sequence.
        publish("accepted-" + sequence + ".json", value);
        return value;
    }
}
