// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.kongweiguang.ja.agentscope;

import java.io.File;
import java.io.IOException;
import java.net.URI;
import java.nio.channels.SeekableByteChannel;
import java.nio.file.CopyOption;
import java.nio.file.DirectoryStream;
import java.nio.file.FileStore;
import java.nio.file.FileSystem;
import java.nio.file.FileSystemLoopException;
import java.nio.file.FileSystemNotFoundException;
import java.nio.file.LinkOption;
import java.nio.file.NoSuchFileException;
import java.nio.file.OpenOption;
import java.nio.file.Path;
import java.nio.file.ProviderMismatchException;
import java.nio.file.WatchEvent;
import java.nio.file.WatchKey;
import java.nio.file.WatchService;
import java.nio.file.attribute.BasicFileAttributes;
import java.nio.file.attribute.FileAttribute;
import java.nio.file.attribute.FileAttributeView;
import java.nio.file.attribute.FileTime;
import java.nio.file.attribute.UserPrincipalLookupService;
import java.nio.file.spi.FileSystemProvider;
import java.util.Collections;
import java.util.Iterator;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.Set;

/**
 * Non-host path used solely because AgentScope 2.0.2 requires a workspace Path even when all
 * workspace capabilities are disabled. Its provider rejects every I/O operation, so a user-created
 * regular file or directory with the sentinel name cannot be read, created, or traversed.
 */
final class InMemoryWorkspacePath implements Path {
    private static final InMemoryWorkspaceFileSystem FILE_SYSTEM = new InMemoryWorkspaceFileSystem();
    private static final String ROOT_VALUE = "/";
    private static final String SENTINEL_VALUE = "/ja-in-memory-workspace";
    private final String value;

    private InMemoryWorkspacePath(String value) {
        this.value = normalizeValue(value);
    }

    /** Returns the process-local sentinel without consulting the default host filesystem. */
    static Path path() {
        return new InMemoryWorkspacePath(SENTINEL_VALUE);
    }

    /** Keeps path operations inside a synthetic absolute namespace. */
    private static String normalizeValue(String raw) {
        String value = Objects.requireNonNull(raw, "raw").replace('\\', '/');
        if (!value.startsWith("/")) {
            value = "/" + value;
        }
        while (value.contains("//")) {
            value = value.replace("//", "/");
        }
        if (value.length() > 1 && value.endsWith("/")) {
            value = value.substring(0, value.length() - 1);
        }
        return value;
    }

    /** Returns path segments without exposing a host path. */
    private List<String> names() {
        if (ROOT_VALUE.equals(value)) {
            return List.of();
        }
        return List.of(value.substring(1).split("/"));
    }

    /** Requires all operations to stay within this synthetic provider. */
    private InMemoryWorkspacePath checked(Path other) {
        if (!(other instanceof InMemoryWorkspacePath path)) {
            throw new ProviderMismatchException("path provider mismatch");
        }
        return path;
    }

    /** Creates a child path in the synthetic namespace. */
    private InMemoryWorkspacePath child(String child) {
        return new InMemoryWorkspacePath(ROOT_VALUE.equals(value) ? "/" + child
                : value + "/" + child);
    }

    @Override
    public FileSystem getFileSystem() {
        return FILE_SYSTEM;
    }

    @Override
    public boolean isAbsolute() {
        return true;
    }

    @Override
    public Path getRoot() {
        return new InMemoryWorkspacePath(ROOT_VALUE);
    }

    @Override
    public Path getFileName() {
        List<String> names = names();
        return names.isEmpty() ? null : new InMemoryWorkspacePath(names.getLast());
    }

    @Override
    public Path getParent() {
        if (ROOT_VALUE.equals(value)) {
            return null;
        }
        int separator = value.lastIndexOf('/');
        return separator == 0 ? getRoot() : new InMemoryWorkspacePath(value.substring(0, separator));
    }

    @Override
    public int getNameCount() {
        return names().size();
    }

    @Override
    public Path getName(int index) {
        return new InMemoryWorkspacePath(names().get(index));
    }

    @Override
    public Path subpath(int beginIndex, int endIndex) {
        return new InMemoryWorkspacePath(String.join("/", names().subList(beginIndex, endIndex)));
    }

    @Override
    public boolean startsWith(Path other) {
        return value.startsWith(checked(other).value);
    }

    @Override
    public boolean endsWith(Path other) {
        return value.endsWith(checked(other).value);
    }

    @Override
    public Path normalize() {
        return this;
    }

    @Override
    public Path resolve(Path other) {
        InMemoryWorkspacePath path = checked(other);
        // FileSystem.getPath receives the relative string passed by WorkspaceManager; the
        // synthetic implementation normalizes it for storage, so restore child semantics here.
        if (path.value.indexOf('/', 1) < 0) {
            return child(path.value.substring(1));
        }
        return path.isAbsolute() ? path : child(path.value);
    }

    @Override
    public Path relativize(Path other) {
        InMemoryWorkspacePath path = checked(other);
        if (value.equals(path.value)) {
            return new InMemoryWorkspacePath(ROOT_VALUE);
        }
        String prefix = value.endsWith("/") ? value : value + "/";
        if (path.value.startsWith(prefix)) {
            return new InMemoryWorkspacePath(path.value.substring(prefix.length()));
        }
        throw new IllegalArgumentException("synthetic paths are not related");
    }

    @Override
    public URI toUri() {
        return URI.create("ja-memory:" + value);
    }

    @Override
    public Path toAbsolutePath() {
        return this;
    }

    @Override
    public Path toRealPath(LinkOption... options) throws IOException {
        throw new NoSuchFileException(value);
    }

    @Override
    public File toFile() {
        throw new UnsupportedOperationException("synthetic workspace has no host File");
    }

    @Override
    public WatchKey register(WatchService watcher, WatchEvent.Kind<?>[] events,
                             WatchEvent.Modifier... modifiers) throws IOException {
        throw new UnsupportedOperationException("synthetic workspace has no watcher");
    }

    @Override
    public Iterator<Path> iterator() {
        return names().stream().map(InMemoryWorkspacePath::new).map(Path.class::cast).iterator();
    }

    @Override
    public int compareTo(Path other) {
        return value.compareTo(checked(other).value);
    }

    @Override
    public boolean equals(Object other) {
        return other instanceof InMemoryWorkspacePath path && value.equals(path.value);
    }

    @Override
    public int hashCode() {
        return value.hashCode();
    }

    @Override
    public String toString() {
        return value;
    }

    /** Synthetic provider that denies all host filesystem operations. */
    private static final class InMemoryWorkspaceProvider extends FileSystemProvider {
        @Override
        public String getScheme() {
            return "ja-memory";
        }

        @Override
        public FileSystem newFileSystem(URI uri, Map<String, ?> env) throws IOException {
            return FILE_SYSTEM;
        }

        @Override
        public FileSystem getFileSystem(URI uri) {
            if (!"ja-memory".equalsIgnoreCase(uri.getScheme())) {
                throw new FileSystemNotFoundException(uri.toString());
            }
            return FILE_SYSTEM;
        }

        @Override
        public Path getPath(URI uri) {
            if (!"ja-memory".equalsIgnoreCase(uri.getScheme())) {
                throw new FileSystemNotFoundException(uri.toString());
            }
            return new InMemoryWorkspacePath(uri.getPath());
        }

        @Override
        public SeekableByteChannel newByteChannel(Path path, Set<? extends OpenOption> options,
                                                   FileAttribute<?>... attrs) throws IOException {
            throw unsupported(path);
        }

        @Override
        public DirectoryStream<Path> newDirectoryStream(Path dir,
                                                         DirectoryStream.Filter<? super Path> filter)
                throws IOException {
            throw unsupported(dir);
        }

        @Override
        public void createDirectory(Path dir, FileAttribute<?>... attrs) throws IOException {
            throw unsupported(dir);
        }

        @Override
        public void delete(Path path) throws IOException {
            throw unsupported(path);
        }

        @Override
        public void copy(Path source, Path target, CopyOption... options) throws IOException {
            throw unsupported(source);
        }

        @Override
        public void move(Path source, Path target, CopyOption... options) throws IOException {
            throw unsupported(source);
        }

        @Override
        public boolean isSameFile(Path path, Path path2) throws IOException {
            return path.equals(path2);
        }

        @Override
        public boolean isHidden(Path path) throws IOException {
            return false;
        }

        @Override
        public FileStore getFileStore(Path path) throws IOException {
            throw unsupported(path);
        }

        @Override
        public void checkAccess(Path path, java.nio.file.AccessMode... modes) throws IOException {
            throw unsupported(path);
        }

        @Override
        public <V extends FileAttributeView> V getFileAttributeView(Path path, Class<V> type,
                                                                      LinkOption... options) {
            return null;
        }

        @Override
        public <A extends BasicFileAttributes> A readAttributes(Path path, Class<A> type,
                                                                 LinkOption... options)
                throws IOException {
            if (type != BasicFileAttributes.class || !(path instanceof InMemoryWorkspacePath value)) {
                throw new NoSuchFileException(path.toString());
            }
            boolean workspace = SENTINEL_VALUE.equals(value.value);
            boolean agents = (SENTINEL_VALUE + "/AGENTS.md").equals(value.value);
            if (!workspace && !agents) {
                throw new NoSuchFileException(path.toString());
            }
            return type.cast(new SyntheticAttributes(workspace));
        }

        @Override
        public Map<String, Object> readAttributes(Path path, String attributes,
                                                  LinkOption... options) throws IOException {
            throw new NoSuchFileException(path.toString());
        }

        @Override
        public void setAttribute(Path path, String attribute, Object value,
                                  LinkOption... options) throws IOException {
            throw unsupported(path);
        }

        /** Returns an IOException rather than delegating any operation to the host provider. */
        private static IOException unsupported(Path path) {
            return new FileSystemLoopException(path.toString());
        }

        /** Supplies fixed metadata so AgentScope validation never falls back to host metadata. */
        private static final class SyntheticAttributes implements BasicFileAttributes {
            private final boolean directory;

            private SyntheticAttributes(boolean directory) {
                this.directory = directory;
            }

            @Override
            public FileTime lastModifiedTime() {
                return FileTime.fromMillis(0);
            }

            @Override
            public FileTime lastAccessTime() {
                return FileTime.fromMillis(0);
            }

            @Override
            public FileTime creationTime() {
                return FileTime.fromMillis(0);
            }

            @Override
            public boolean isRegularFile() {
                return !directory;
            }

            @Override
            public boolean isDirectory() {
                return directory;
            }

            @Override
            public boolean isSymbolicLink() {
                return false;
            }

            @Override
            public boolean isOther() {
                return false;
            }

            @Override
            public long size() {
                return 0;
            }

            @Override
            public Object fileKey() {
                return null;
            }
        }
    }

    /** Minimal read-only FileSystem used by the synthetic sentinel path. */
    private static final class InMemoryWorkspaceFileSystem extends FileSystem {
        private static final InMemoryWorkspaceProvider PROVIDER = new InMemoryWorkspaceProvider();

        @Override
        public FileSystemProvider provider() {
            return PROVIDER;
        }

        @Override
        public void close() {
        }

        @Override
        public boolean isOpen() {
            return true;
        }

        @Override
        public boolean isReadOnly() {
            return true;
        }

        @Override
        public String getSeparator() {
            return "/";
        }

        @Override
        public Iterable<Path> getRootDirectories() {
            return List.of(new InMemoryWorkspacePath(ROOT_VALUE));
        }

        @Override
        public Iterable<FileStore> getFileStores() {
            return List.of();
        }

        @Override
        public Set<String> supportedFileAttributeViews() {
            return Collections.emptySet();
        }

        @Override
        public Path getPath(String first, String... more) {
            StringBuilder value = new StringBuilder(first);
            for (String part : more) {
                if (!value.isEmpty() && value.charAt(value.length() - 1) != '/') {
                    value.append('/');
                }
                value.append(part);
            }
            return new InMemoryWorkspacePath(value.toString());
        }

        @Override
        public java.nio.file.PathMatcher getPathMatcher(String syntaxAndPattern) {
            throw new UnsupportedOperationException("synthetic workspace has no matcher");
        }

        @Override
        public UserPrincipalLookupService getUserPrincipalLookupService() {
            throw new UnsupportedOperationException("synthetic workspace has no principals");
        }

        @Override
        public WatchService newWatchService() throws IOException {
            throw new UnsupportedOperationException("synthetic workspace has no watcher");
        }
    }
}
