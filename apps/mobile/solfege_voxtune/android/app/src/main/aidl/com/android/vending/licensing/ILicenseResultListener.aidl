// Google Play's answer to ILicensingService.checkLicense: a response code,
// and for the licensed / not licensed answers the signed data and its
// signature, made with the app's licensing key.
package com.android.vending.licensing;

oneway interface ILicenseResultListener {
    void verifyLicense(int responseCode, String signedData, String signature);
}
