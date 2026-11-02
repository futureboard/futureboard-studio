// Google Play's licensing service, as the Play Store app exposes it (the
// same interface as Google's License Verification Library).
package com.android.vending.licensing;

import com.android.vending.licensing.ILicenseResultListener;

oneway interface ILicensingService {
    void checkLicense(long nonce, String packageName, in ILicenseResultListener listener);
}
