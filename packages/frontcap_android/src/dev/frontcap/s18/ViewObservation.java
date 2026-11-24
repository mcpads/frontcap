package dev.frontcap.s18;

import android.app.Activity;
import android.graphics.Rect;
import android.text.method.PasswordTransformationMethod;
import android.view.View;
import android.view.ViewGroup;
import android.view.ViewParent;
import android.view.WindowInsets;
import android.widget.*;
import java.util.ArrayDeque;
import java.util.IdentityHashMap;
import java.util.UUID;
import org.json.JSONArray;
import org.json.JSONObject;

/** Source View state only: no hit-test, accessibility activation or input authority. */
final class ViewObservation {
  private IdentityHashMap<View,String> previous = new IdentityHashMap<>();

  private static String bounded(CharSequence value, int limit) {
    String text = value == null ? "" : value.toString();
    if (text.length() > limit) throw new IllegalStateException("text_budget_exceeded");
    return text;
  }
  static boolean shown(View view) {
    for (View current = view; current != null;) {
      if (current.getVisibility() != View.VISIBLE || (!Float.isFinite(current.getAlpha()) || current.getAlpha() <= 0)) return false;
      ViewParent parent = current.getParent();
      current = parent instanceof View ? (View)parent : null;
    }
    return view.isShown() && view.getGlobalVisibleRect(new Rect());
  }
  static Rect visibleBoundsScreen(View view) {
    Rect rect=new Rect();
    if (!view.isShown() || !view.getLocalVisibleRect(rect)) {rect.setEmpty();return rect;}
    int[] position=new int[2];view.getLocationOnScreen(position);
    // getLocalVisibleRect includes this View's own content scroll; screen location does not.
    rect.offset(position[0]-view.getScrollX(),position[1]-view.getScrollY());
    Rect window=new Rect();view.getWindowVisibleDisplayFrame(window);
    if (!rect.intersect(window)) rect.setEmpty();
    return rect;
  }
  static boolean transformed(View view) {
    for (View current=view; current!=null;) {
      if (!current.getMatrix().isIdentity()) return true;
      ViewParent parent=current.getParent(); current=parent instanceof View ? (View)parent : null;
    }
    return false;
  }
  java.util.Set<View> targets() { return previous.keySet(); }
  View resolve(String token) {
    for (java.util.Map.Entry<View,String> entry:previous.entrySet())
      if (entry.getValue().equals(token)) return entry.getKey();
    return null;
  }
  static int editLimit(EditText view) {
    int limit=16384;
    for (android.text.InputFilter filter:view.getFilters())
      if (filter instanceof android.text.InputFilter.LengthFilter)
        limit=Math.min(limit,((android.text.InputFilter.LengthFilter)filter).getMax());
    return limit;
  }
  static boolean editable(View view) {
    return view instanceof EditText && ((EditText)view).getKeyListener()!=null
        && view.isEnabled() && shown(view) && !transformed(view)
        && !(((EditText)view).getTransformationMethod() instanceof PasswordTransformationMethod);
  }
  private static String role(View view) {
    if (view instanceof EditText) return "text_field";
    if (view instanceof Switch) return "switch";
    if (view instanceof CheckBox) return "check_box";
    if (view instanceof Button) return "button";
    if (view instanceof TextView) return "static_text";
    if (view instanceof ScrollView || view instanceof HorizontalScrollView || view instanceof AbsListView) return "scroll_view";
    return "view";
  }
  JSONObject read(Activity app) throws Exception {
    View root = app.getWindow().getDecorView();
    if (!root.isAttachedToWindow() || root.getWidth() <= 0 || root.getHeight() <= 0)
      throw new IllegalStateException("window_not_laid_out");
    JSONArray targets = new JSONArray();
    IdentityHashMap<View,String> current = new IdentityHashMap<>();
    ArrayDeque<View> pending = new ArrayDeque<>(); pending.add(root);
    while (!pending.isEmpty()) {
      View view = pending.removeFirst();
      if (current.size() >= 2048) throw new IllegalStateException("view_budget_exceeded");
      String token = previous.get(view);
      if (token == null) token = UUID.randomUUID().toString();
      current.put(view,token);
      int[] position = new int[2]; view.getLocationOnScreen(position);
      String resource = "";
      if (view.getId() != View.NO_ID) {
        try { resource = view.getResources().getResourceName(view.getId()); }
        catch (android.content.res.Resources.NotFoundException ignored) { /* Generated IDs are not selectors. */ }
      }
      TextView text = view instanceof TextView ? (TextView)view : null;
      boolean secure = text != null && text.getTransformationMethod() instanceof PasswordTransformationMethod;
      JSONObject row = new JSONObject().put("token",token).put("class_name",view.getClass().getName())
          .put("role",role(view)).put("resource_id",bounded(resource,4096))
          .put("label",bounded(view.getContentDescription(),4096))
          .put("text",text == null || secure ? JSONObject.NULL : bounded(text.getText(),16384))
          .put("error",text == null || secure || text.getError() == null ? JSONObject.NULL : bounded(text.getError(),4096))
          .put("edit_limit_utf16",view instanceof EditText && !secure && ((EditText)view).getKeyListener()!=null
              ? editLimit((EditText)view) : JSONObject.NULL)
          .put("secure",secure).put("enabled",view.isEnabled()).put("visible",shown(view))
          .put("focused",view.isFocused()).put("clickable",view.isClickable()).put("selected",view.isSelected())
          .put("transformed",transformed(view)).put("layout_bounds_screen_px",new JSONArray(new long[]{position[0],position[1],
              (long)position[0]+view.getWidth(),(long)position[1]+view.getHeight()}));
      targets.put(row);
      if (view instanceof ViewGroup) {
        ViewGroup group = (ViewGroup)view;
        for (int i=0;i<group.getChildCount();i++) pending.addLast(group.getChildAt(i));
      }
    }
    previous = current;
    for (int i=0;i<targets.length();i++) {
      JSONObject row=targets.getJSONObject(i);
      JSONArray point=ViewTapping.point(resolve(row.getString("token")),this);
      row.put("tap_point_screen_px",point==null ? JSONObject.NULL : point);
      JSONObject scroll=ViewScrolling.geometry(resolve(row.getString("token")),this);
      row.put("scroll",scroll==null ? JSONObject.NULL : scroll);
    }
    WindowInsets insets = root.getRootWindowInsets();
    JSONObject result = new JSONObject().put("window_focused",root.hasWindowFocus())
        .put("ime_visible",insets == null ? JSONObject.NULL : insets.isVisible(WindowInsets.Type.ime()))
        .put("targets",targets);
    previous = current;
    return result;
  }
}
