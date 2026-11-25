import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.PrintWriter;
import java.io.RandomAccessFile;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.UUID;

/** Probe port 15060 once before capture and once after restore. The UUID must
 * match and the counter must advance. The peer only connects on startup. */
public final class HeapProbe {
    public static void main(String[] args) throws Exception {
        InetAddress loop = InetAddress.getLoopbackAddress();
        if (args.length > 0) {
            UUID token = UUID.randomUUID();
            new Thread(() -> {
                try (RandomAccessFile heartbeat = new RandomAccessFile("/tmp/heartbeat", "rw")) {
                    for (long n = 1; ; n++) {
                        heartbeat.seek(0);
                        heartbeat.writeLong(n);
                        Thread.sleep(3);
                    }
                } catch (Exception e) { throw new RuntimeException(e); }
            }).start();
            try (RandomAccessFile file = new RandomAccessFile("/tmp/continuity.dat", "rwd");
                 Socket socket = new Socket(loop, 15061)) {
                DataInputStream in = new DataInputStream(socket.getInputStream());
                DataOutputStream out = new DataOutputStream(socket.getOutputStream());
                for (long n = 1; ; n++) {
                    in.readByte();
                    long offset = file.getFilePointer();
                    if (offset != (n - 1) * Long.BYTES || file.length() != offset) throw new IOException("lost open file position");
                    if (n > 1) {
                        file.seek(offset - Long.BYTES);
                        if (file.readLong() != n - 1) throw new IOException("lost durable write");
                    }
                    file.writeLong(n);
                    out.writeUTF(token + " " + n);
                    out.flush();
                }
            }
        }
        try (ServerSocket backend = new ServerSocket(15061, 1, loop)) {
            new ProcessBuilder(System.getProperty("java.home") + "/bin/java", "-Xmx24m", "-cp", System.getProperty("java.class.path"), "HeapProbe", "peer").inheritIO().start();
            try (Socket persistent = backend.accept(); ServerSocket listener = new ServerSocket(15060, 8, loop)) {
                DataInputStream in = new DataInputStream(persistent.getInputStream());
                DataOutputStream out = new DataOutputStream(persistent.getOutputStream());
                while (true) {
                    try (Socket client = listener.accept(); PrintWriter reply = new PrintWriter(client.getOutputStream(), true)) {
                        out.writeByte(1);
                        out.flush();
                        reply.println(in.readUTF());
                    }
                }
            }
        }
    }
}
