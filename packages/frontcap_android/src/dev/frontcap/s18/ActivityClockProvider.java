package dev.frontcap.s18;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;

/** Experimental DUMP-protected clock submission; the durable clock receipt is authoritative. */
public class ActivityClockProvider extends ContentProvider {
  private static volatile ClockControl active;

  public static void bind(ClockControl clock) { active = clock; }
  public static void unbind(ClockControl clock) { if (active == clock) active = null; }
  @Override public boolean onCreate() { return true; }

  @Override public Bundle call(String method, String arg, Bundle extras) {
    // ContentProvider.call does not imply CRUD permission checks; enforce this explicitly.
    getContext().enforceCallingPermission("android.permission.DUMP", "Controlled clock requires DUMP");
    if (!"execute".equals(method) || arg != null || extras == null)
      throw new IllegalArgumentException("Unsupported clock submission");
    final ClockControl clock = active;
    if (clock == null) throw new IllegalStateException("Controlled Activity is not ready");
    final Intent request = new Intent().putExtras(new Bundle(extras));
    boolean posted = new Handler(Looper.getMainLooper()).post(() -> {
      // A queued submission may outlive the Activity, but cannot enter its replacement.
      if (active != clock) { Log.i("FrontcapClockSubmission", "discarded retired Activity submission"); return; }
      clock.execute(request, "provider", (code, data) ->
          Log.i("FrontcapClockSubmission", "result=" + code + " data=" + data));
    });
    if (!posted) throw new IllegalStateException("Activity main queue rejected submission");
    Bundle reply = new Bundle();
    reply.putBoolean("submitted", true);
    return reply;
  }

  @Override public Cursor query(Uri uri, String[] projection, String selection,
      String[] selectionArgs, String sortOrder) { throw new UnsupportedOperationException(); }
  @Override public String getType(Uri uri) { throw new UnsupportedOperationException(); }
  @Override public Uri insert(Uri uri, ContentValues values) { throw new UnsupportedOperationException(); }
  @Override public int delete(Uri uri, String selection, String[] selectionArgs) { throw new UnsupportedOperationException(); }
  @Override public int update(Uri uri, ContentValues values, String selection, String[] selectionArgs) { throw new UnsupportedOperationException(); }
}
