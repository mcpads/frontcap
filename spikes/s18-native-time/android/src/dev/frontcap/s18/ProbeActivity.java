package dev.frontcap.s18;

import android.app.Activity;
import android.os.Bundle;
import android.graphics.Color;
import android.view.View;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;

// Installed-app input/observation baseline. Uses ordinary device time, separate from clock injection.
public class ProbeActivity extends Activity {
  private int count;
  @Override public void onCreate(Bundle saved) {
    super.onCreate(saved);
    LinearLayout layout = new LinearLayout(this);
    layout.setOrientation(LinearLayout.VERTICAL);
    layout.setPadding(32, 120, 32, 32);
    layout.setBackgroundColor(Color.WHITE);
    TextView label = new TextView(this);
    label.setTextSize(28);
    label.setTextColor(Color.BLACK);
    label.setText("Count: 0");
    label.setContentDescription("native-count-0");
    layout.addView(label);
    View color = new View(this);
    color.setContentDescription("native-color");
    color.setBackgroundColor(Color.RED);
    layout.addView(color, new LinearLayout.LayoutParams(240, 160));
    Button increment = new Button(this);
    increment.setText("Increment");
    increment.setContentDescription("native-increment");
    increment.setOnClickListener(view -> {
      count++;
      label.setText("Count: " + count);
      label.setContentDescription("native-count-" + count);
      color.setBackgroundColor(Color.GREEN);
    });
    layout.addView(increment);
    setContentView(layout);
  }
}
