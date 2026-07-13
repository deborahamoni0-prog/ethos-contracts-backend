package com.ethosprotocol.services

import android.app.Activity
import androidx.credentials.*
import com.ethosprotocol.api.ApiClient
import com.ethosprotocol.api.ApiResult
import com.ethosprotocol.api.TokenProvider
import com.ethosprotocol.models.PasskeyRegisterRequest
import com.ethosprotocol.models.PasskeyVerifyRequest
import org.json.JSONArray
import org.json.JSONObject
import java.util.Base64
import javax.inject.Inject
import javax.inject.Singleton

@Singleton
class PasskeyService @Inject constructor(
    private val apiClient: ApiClient,
    private val tokenProvider: TokenProvider
) {
    suspend fun register(activity: Activity, username: String): Result<Unit> = runCatching {
        val challenge = requireSuccess(apiClient.getChallenge()).challenge
        val requestJson = JSONObject().apply {
            put("challenge", challenge)
            put("rp", JSONObject().put("id", "ethos-protocol.app").put("name", "Ethos-Protocol"))
            put("user", JSONObject()
                .put("id", Base64.getUrlEncoder().withoutPadding().encodeToString(username.toByteArray()))
                .put("name", username).put("displayName", username))
            put("pubKeyCredParams", JSONArray().put(JSONObject().put("type", "public-key").put("alg", -7)))
            put("authenticatorSelection", JSONObject()
                .put("authenticatorAttachment", "platform")
                .put("requireResidentKey", true)
                .put("userVerification", "required"))
        }.toString()

        val credManager = CredentialManager.create(activity)
        val resp = credManager.createCredential(activity, CreatePublicKeyCredentialRequest(requestJson))
                as CreatePublicKeyCredentialResponse
        val json = JSONObject(resp.registrationResponseJson)
        val regReq = PasskeyRegisterRequest(
            credentialId = json.getString("id"),
            publicKey = json.getJSONObject("response").getString("attestationObject"),
            clientDataJson = json.getJSONObject("response").getString("clientDataJSON")
        )
        requireSuccess(apiClient.registerPasskey(regReq))
    }

    suspend fun authenticate(activity: Activity): Result<Unit> = runCatching {
        val challenge = requireSuccess(apiClient.getChallenge()).challenge
        val requestJson = JSONObject()
            .put("challenge", challenge).put("rpId", "ethos-protocol.app")
            .put("userVerification", "required").toString()

        val credManager = CredentialManager.create(activity)
        val request = GetCredentialRequest(listOf(GetPublicKeyCredentialOption(requestJson)))
        val credential = credManager.getCredential(activity, request).credential as PublicKeyCredential
        val json = JSONObject(credential.authenticationResponseJson)
        val verifyReq = PasskeyVerifyRequest(
            credentialId = json.getString("id"),
            clientDataJson = json.getJSONObject("response").getString("clientDataJSON"),
            signature = json.getJSONObject("response").getString("signature")
        )
        tokenProvider.token = requireSuccess(apiClient.verifyPasskey(verifyReq)).token
    }

    private fun <T> requireSuccess(result: ApiResult<T>): T {
        return when (result) {
            is ApiResult.Success -> result.data
            is ApiResult.Error -> error(result.message)
            ApiResult.NetworkUnavailable -> error("No network connection")
        }
    }
}
