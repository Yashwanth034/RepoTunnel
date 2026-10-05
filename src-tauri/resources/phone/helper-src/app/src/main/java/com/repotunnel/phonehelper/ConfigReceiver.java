package com.repotunnel.phonehelper;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

public final class ConfigReceiver extends BroadcastReceiver {
    static final String ACTION_CONFIGURE = "com.repotunnel.phonehelper.CONFIGURE";
    static final String PREFS = "repotunnel_phone_helper";
    static final String NONCE_KEY = "session_nonce";

    @Override
    public void onReceive(Context context, Intent intent) {
        if (intent == null || !ACTION_CONFIGURE.equals(intent.getAction())) {
            return;
        }
        String nonce = intent.getStringExtra("nonce");
        if (nonce == null || nonce.length() != 64) {
            setResultCode(2);
            return;
        }
        for (int index = 0; index < nonce.length(); index++) {
            char value = nonce.charAt(index);
            boolean hex = (value >= '0' && value <= '9')
                    || (value >= 'a' && value <= 'f')
                    || (value >= 'A' && value <= 'F');
            if (!hex) {
                setResultCode(2);
                return;
            }
        }
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
                .edit()
                .putString(NONCE_KEY, nonce.toLowerCase())
                .apply();
        PhoneAccessibilityService.onTokenUpdated();
        setResultCode(0);
    }
}
