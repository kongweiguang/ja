// @author kongweiguang
// SPDX-License-Identifier: GPL-3.0-or-later

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.io.PrintWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * 仅用于验证 Rust host 能以真实 JVM executable 管理 Java stdio sidecar。
 * 该 fixture 不依赖 AgentScope/Solon，避免把 spike 变成生产运行时测试。
 */
public final class JaFixture {
    /**
     * 通过协议 barrier 驱动 ready/shutdown，保证测试不依赖任意延时。
     */
    public static void main(String[] args) throws Exception {
        String pidFile = value(args, "--pid-file");
        if (pidFile != null) {
            Files.writeString(Path.of(pidFile), Long.toString(ProcessHandle.current().pid()), StandardCharsets.UTF_8);
        }
        String mode = value(args, "--mode");
        if (mode == null) {
            mode = "normal";
        }
        BufferedReader reader = new BufferedReader(new InputStreamReader(System.in, StandardCharsets.UTF_8));
        PrintWriter writer = new PrintWriter(System.out, true, StandardCharsets.UTF_8);
        String line;
        boolean ready = false;
        while ((line = reader.readLine()) != null) {
            if (!ready && line.contains("\"method\":\"initialize\"")) {
                if ("incompatible".equals(mode)) {
                    writer.println("{\"jsonrpc\":\"2.0\",\"id\":\"c:initialize\",\"error\":{\"code\":-32003,\"data\":{\"jaCode\":\"PROTOCOL_VERSION_UNSUPPORTED\"}}}");
                    return;
                }
                writer.println("{\"jsonrpc\":\"2.0\",\"id\":\"c:initialize\",\"result\":{\"protocolMajor\":1,\"protocolMinor\":0,\"serverInstanceId\":\"srv_java_fixture\"}}");
                writer.println("{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{}}");
                writer.println("{\"jsonrpc\":\"2.0\",\"method\":\"runtime/statusChanged\",\"params\":{\"status\":\"ready\"}}");
                ready = true;
            } else if (ready && line.contains("\"method\":\"shutdown\"")) {
                writer.println("{\"jsonrpc\":\"2.0\",\"id\":\"c:shutdown\",\"result\":{\"accepted\":true}}");
                return;
            }
        }
    }

    /**
     * 只读取预先声明的参数，避免 fixture 示例暗示 shell 拼接是合法 sidecar API。
     */
    private static String value(String[] args, String name) {
        for (int index = 0; index + 1 < args.length; index++) {
            if (name.equals(args[index])) {
                return args[index + 1];
            }
        }
        return null;
    }
}
