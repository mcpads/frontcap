package dev.frontcap.input;

import android.os.Process;
import android.os.SystemClock;
import android.view.InputDevice;
import android.view.InputEvent;
import android.view.MotionEvent;
import org.json.JSONArray;
import org.json.JSONObject;
import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.lang.reflect.Method;
import java.util.ArrayList;
import java.util.Map;
import java.util.TreeMap;
import java.util.UUID;

/** Bounded shell-owned input experiment; never loaded into the consumer app. */
public final class TouchInput {
    private final String generation = UUID.randomUUID().toString();
    private final TreeMap<Integer, float[]> contacts = new TreeMap<>();
    private final Object manager;
    private final Method inject;
    private long downTime;
    private int revision;
    private boolean dispatched;

    private TouchInput() throws Exception {
        Class<?> type = Class.forName("android.hardware.input.InputManagerGlobal");
        manager = type.getMethod("getInstance").invoke(null);
        inject = type.getMethod("injectInputEvent", InputEvent.class, int.class);
    }

    private static double number(Object value, double min, double max) {
        if (!(value instanceof Number)) throw new IllegalArgumentException("Expected number");
        double n = ((Number)value).doubleValue();
        if (!Double.isFinite(n) || n < min || n > max)
            throw new IllegalArgumentException("Number outside bounds");
        return n;
    }

    private static int integer(Object value, int min, int max) {
        double n = number(value, min, max);
        if (n != Math.floor(n)) throw new IllegalArgumentException("Expected integer");
        return (int)n;
    }

    private static TreeMap<Integer, float[]> parseContacts(JSONArray list) throws Exception {
        if (list.length() > 5) throw new IllegalArgumentException("Too many contacts");
        TreeMap<Integer, float[]> next = new TreeMap<>();
        for (int i = 0; i < list.length(); i++) {
            JSONObject point = list.getJSONObject(i);
            if (point.length() != 3) throw new IllegalArgumentException("Unknown contact fields");
            int id = integer(point.get("id"), 1, 5);
            float x = (float)number(point.get("x"), 0, 16384);
            float y = (float)number(point.get("y"), 0, 16384);
            if (next.put(id, new float[]{x,y}) != null)
                throw new IllegalArgumentException("Duplicate contact ID");
        }
        return next;
    }

    private JSONArray snapshot() throws Exception {
        JSONArray result = new JSONArray();
        for (Map.Entry<Integer,float[]> entry : contacts.entrySet())
            result.put(new JSONObject().put("id",entry.getKey())
                .put("x",entry.getValue()[0]).put("y",entry.getValue()[1]));
        return result;
    }

    private void send(int action, JSONArray frames) throws Exception {
        send(action, frames, Long.MAX_VALUE);
    }

    private void send(int action, JSONArray frames, long maximumAge) throws Exception {
        int count = contacts.size();
        MotionEvent.PointerProperties[] properties = new MotionEvent.PointerProperties[count];
        MotionEvent.PointerCoords[] coordinates = new MotionEvent.PointerCoords[count];
        int index = 0;
        for (Map.Entry<Integer,float[]> entry : contacts.entrySet()) {
            properties[index] = new MotionEvent.PointerProperties();
            properties[index].id = entry.getKey();
            properties[index].toolType = MotionEvent.TOOL_TYPE_FINGER;
            coordinates[index] = new MotionEvent.PointerCoords();
            coordinates[index].x = entry.getValue()[0];
            coordinates[index].y = entry.getValue()[1];
            coordinates[index].pressure = 1;
            coordinates[index].size = 1;
            index++;
        }
        long now = SystemClock.uptimeMillis();
        if (now - downTime > maximumAge)
            throw new IllegalStateException("Short tap exceeded native event age");
        MotionEvent event = MotionEvent.obtain(downTime, now, action, count, properties, coordinates,
            0, 0, 1, 1, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0);
        JSONObject receipt = new JSONObject().put("action",action).put("uptime_ms",now)
            .put("down_time_ms",downTime).put("contacts",snapshot());
        frames.put(receipt);
        try {
            dispatched = true; // An exception after this point is unknown, never replayable.
            boolean accepted = (Boolean)inject.invoke(manager,event,2); // WAIT_FOR_FINISH.
            receipt.put("accepted",accepted);
            if (!accepted) throw new IllegalStateException("Native injection did not finish");
        } finally { event.recycle(); }
    }

    private void apply(TreeMap<Integer,float[]> next, boolean cancel, JSONArray frames) throws Exception {
        if (cancel) {
            if (!contacts.isEmpty()) send(MotionEvent.ACTION_CANCEL,frames);
            contacts.clear();
            return;
        }
        // End absent IDs before adding new ones. Index is calculated in each actual event array.
        for (int id : new ArrayList<>(contacts.keySet())) {
            if (next.containsKey(id)) continue;
            int index = new ArrayList<>(contacts.keySet()).indexOf(id);
            send(contacts.size() == 1 ? MotionEvent.ACTION_UP :
                MotionEvent.ACTION_POINTER_UP | (index << MotionEvent.ACTION_POINTER_INDEX_SHIFT),frames);
            contacts.remove(id);
        }
        boolean moved = false;
        for (int id : new ArrayList<>(contacts.keySet())) {
            float[] old = contacts.get(id), point = next.get(id);
            moved |= old[0] != point[0] || old[1] != point[1];
            contacts.put(id,point);
        }
        if (moved) send(MotionEvent.ACTION_MOVE,frames);
        for (Map.Entry<Integer,float[]> entry : next.entrySet()) {
            if (contacts.containsKey(entry.getKey())) continue;
            if (contacts.isEmpty()) downTime = SystemClock.uptimeMillis();
            contacts.put(entry.getKey(),entry.getValue());
            int index = new ArrayList<>(contacts.keySet()).indexOf(entry.getKey());
            send(contacts.size() == 1 ? MotionEvent.ACTION_DOWN :
                MotionEvent.ACTION_POINTER_DOWN | (index << MotionEvent.ACTION_POINTER_INDEX_SHIFT),frames);
        }
    }

    private void tap(JSONObject point, JSONArray frames) throws Exception {
        if (!contacts.isEmpty()) throw new IllegalArgumentException("Contacts still held");
        if (point.length() != 2) throw new IllegalArgumentException("Unknown tap fields");
        float x = (float)number(point.get("x"), 0, 16384);
        float y = (float)number(point.get("y"), 0, 16384);
        TreeMap<Integer,float[]> next = new TreeMap<>();
        next.put(1, new float[]{x,y});
        apply(next,false,frames);
        // A slow DOWN leaves uncertainty; never forge timestamps or replay a release.
        send(MotionEvent.ACTION_UP,frames,100);
        contacts.clear();
    }

    private static void output(JSONObject value) {
        System.out.println(value);
        System.out.flush();
    }

    public static void main(String[] args) throws Exception {
        if (Process.myUid() != 2000) throw new IllegalStateException("Requires adb shell identity");
        if (args.length > 3) throw new IllegalArgumentException("Expected optional fresh journal directory, lifetime milliseconds and request capacity");
        long lifetime = args.length >= 2 ? Long.parseLong(args[1]) : 300_000;
        int capacity = args.length == 3 ? Integer.parseInt(args[2]) : 24;
        if (capacity < 1 || capacity > 4096) throw new IllegalArgumentException("Capacity must be 1..4096");
        InputFiles files = args.length >= 1
            ? new InputFiles(java.nio.file.Paths.get(args[0]), lifetime) : null;
        TouchInput driver = new TouchInput();
        JSONObject ready = new JSONObject().put("status","ready").put("generation",driver.generation)
            .put("short_tap",true).put("request_capacity",capacity).put("pid",Process.myPid()).put("uid",Process.myUid());
        if (files != null) files.publish("ready.json", ready.toString());
        output(ready);
        BufferedReader reader = new BufferedReader(new InputStreamReader(System.in));
        for (int sequence = 1; sequence <= capacity; sequence++) {
            // Bound before parsing or allocating an unbounded line.
            StringBuilder line = new StringBuilder();
            if (files != null) {
                line.append(files.await(sequence));
            } else {
                for (int ch; (ch = reader.read()) != -1 && ch != '\n';) {
                    if (line.length() >= 8192) throw new IllegalArgumentException("Request too large");
                    line.append((char)ch);
                }
            }
            if (line.length() == 0) break;
            JSONObject reply = new JSONObject().put("generation",driver.generation).put("request_id",sequence);
            JSONArray frames = new JSONArray();
            driver.dispatched = false;
            boolean finish = false;
            try {
                JSONObject request = new JSONObject(line.toString());
                if (request.length() != 4 || !driver.generation.equals(request.get("generation")) ||
                    integer(request.get("request_id"),1,capacity) != sequence ||
                    integer(request.get("expected_revision"),0,capacity) != driver.revision)
                    throw new IllegalArgumentException("Foreign generation, sequence or contact revision");
                boolean cancel = Boolean.TRUE.equals(request.opt("cancel"));
                finish = Boolean.TRUE.equals(request.opt("finish"));
                TreeMap<Integer,float[]> next;
                if (cancel || finish || request.has("tap")) next = new TreeMap<>();
                else next = parseContacts(request.getJSONArray("contacts"));
                if (finish && !driver.contacts.isEmpty())
                    throw new IllegalArgumentException("Contacts still held");
                if (request.has("tap")) driver.tap(request.getJSONObject("tap"),frames);
                else if (!finish) driver.apply(next,cancel,frames);
                driver.revision++;
                reply.put("status",finish ? "finished" : "injected");
            } catch (Exception error) {
                finish = false;
                reply.put("status",driver.dispatched ? "unknown" : "rejected")
                    .put("error",error.toString());
            }
            reply.put("contact_revision",driver.revision).put("contacts",driver.snapshot()).put("frames",frames);
            if (files != null) files.publish("reply-" + sequence + ".json", reply.toString());
            output(reply);
            if (finish || reply.getString("status").equals("unknown")) break;
        }
        // EOF/uncertainty must not invent a cancellation that could commit consumer work.
        if (!driver.contacts.isEmpty())
            System.err.println("Unresolved contacts at exit; retire the owned target, do not replay");
    }
}
