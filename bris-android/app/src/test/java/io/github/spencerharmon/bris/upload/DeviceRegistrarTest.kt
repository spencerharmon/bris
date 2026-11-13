package io.github.spencerharmon.bris.upload

import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

class DeviceRegistrarTest {
    private lateinit var server: MockWebServer

    @Before fun setUp() { server = MockWebServer().also { it.start() } }
    @After fun tearDown() { server.shutdown() }

    private fun base() = server.url("/").toString().trimEnd('/')

    @Test
    fun registersAndReturnsDeviceToken() {
        server.enqueue(MockResponse().setBody("""{"device_uuid":"u1","token":"per-device"}"""))
        val r = DeviceRegistrar().register(base(), "enroll", "u1")
        assertEquals(RegistrationResult.Registered("per-device"), r)
        val req = server.takeRequest()
        assertEquals("POST", req.method)
        assertEquals("/v1/devices/register", req.path)
        assertEquals("Bearer enroll", req.getHeader("Authorization"))
        assertEquals("u1", JSONObject(req.body.readUtf8()).getString("device_uuid"))
    }

    @Test
    fun unauthorizedMeansNotEnrollmentToken() {
        server.enqueue(MockResponse().setResponseCode(401))
        assertEquals(RegistrationResult.NotEnrollmentToken(401), DeviceRegistrar().register(base(), "dev", "u1"))
    }

    @Test
    fun serverErrorAndMissingTokenFail() {
        server.enqueue(MockResponse().setResponseCode(500))
        assertTrue(DeviceRegistrar().register(base(), "e", "u") is RegistrationResult.Failed)
        server.enqueue(MockResponse().setBody("{}"))
        assertTrue(DeviceRegistrar().register(base(), "e", "u") is RegistrationResult.Failed)
    }
}
